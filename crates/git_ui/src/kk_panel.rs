use super::{
    kk_classification::{
        ClassificationCompletion, ClassificationFailure, ClassificationJob, ConflictJob,
        ConflictRequest, FileEditJob, FileMutationAction, FileMutationJob, FileMutationRequest,
        QueuedPreview, RepositoryJob, RepositoryQueue, UpdateStaleJob, WriteDependency,
    },
    kk_comparison::{Comparison, ComparisonIntent, ComparisonState},
    kk_diff::{DiffOpenMode, KkDiff},
    kk_file_order::{FileOrder, FileOrdering},
    kk_multi_diff::KkMultiDiff,
    kk_output::KkOutput,
    kk_revision_picker::{self, Purpose},
    kk_search::{DiffSearch, SearchInChanges, SearchInRevision},
    kk_settings::{CommitInformation, HistoryPreference, KkWorkbenchSettings},
};
use gpui::{
    Action, App, AsyncWindowContext, ClipboardItem, Context, DismissEvent, Entity, EntityId,
    EventEmitter, FocusHandle, Focusable, Pixels, Point, Subscription, Task,
    UniformListScrollHandle, WeakEntity, Window, actions, anchored, deferred, uniform_list,
};
use kk_workbench::{
    Category, ChangedFile, ClassificationTarget, DiffScope, FileMoveDirection, FileSelection,
    HistoryMode, HistoryPage, OperationReceipt, Publication, PublicationStatus, Revision, Snapshot,
    UncertainPublication, discover_with_diagnostics, history_graph,
};
use notifications::status_toast::StatusToast;
use settings::{Settings, SettingsStore};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};
use ui::{ContextMenu, IconButton, ListItem, PopoverMenu, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

#[path = "kk_file_actions.rs"]
mod file_actions;
use file_actions::{FileAction, FileBatchKey, FileRow};
#[path = "kk_file_opening.rs"]
mod file_opening;
pub(super) use file_opening::{FileOpeningState, OpeningToken};
#[path = "kk_tree.rs"]
mod tree;
use tree::{TreeRowId, TreeSelectionState};
#[path = "kk_list_preview.rs"]
mod list_preview;
#[path = "kk_refresh.rs"]
mod refresh;
#[path = "kk_refresh_demand.rs"]
mod refresh_demand;
use futures::StreamExt as _;
use list_preview::{ListPreviews, MutationPreviews};
use refresh::{RefreshEvent, RefreshRequest, RepositoryRefresh};
use refresh_demand::RefreshDemand;

#[cfg(test)]
#[path = "kk_remote_tests.rs"]
pub(super) mod remote_tests;

actions!(
    kk_workbench,
    [
        ToggleFocus,
        Refresh,
        UpdateStaleWorkspace,
        ShowOutput,
        CloseComparison,
        ShowCurrentLineage,
        ShowLogHistory,
        LoadMoreCommits,
        CompareWith,
        FindChange,
        CopyChangeId,
        CopyCommitId,
        RetryCommit,
        SortFilesByName,
        UseRepositoryFileOrder,
        OpenSelectedFiles,
        RevertSelectedFiles,
        MoveSelectedFilesToParent,
        MoveSelectedFilesToChild
    ]
);

#[derive(Clone)]
struct RepositoryView {
    mode: HistoryMode,
    stale: bool,
    snapshot: Snapshot,
    history: HistoryPage,
    expanded: HashMap<String, RevisionFiles>,
    comparison: Option<Comparison>,
    history_targets: Vec<Revision>,
}

impl RepositoryView {
    fn same_content(&self, other: &Self) -> bool {
        self.mode == other.mode
            && self.stale == other.stale
            && self.history == other.history
            && self.expanded == other.expanded
            && self.comparison == other.comparison
            && self.history_targets == other.history_targets
    }
}

#[derive(Clone, PartialEq, Eq)]
enum RevisionFiles {
    Loading,
    Loaded(Vec<ChangedFile>),
    Failed(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum HistoryLoad {
    Loading,
    Failed,
}

#[derive(Clone, Copy, PartialEq)]
pub(super) enum FileOpenKind {
    Current,
    Revision,
    Before,
}

struct KkStatus {
    panel: WeakEntity<KkPanel>,
    _subscription: Subscription,
}

impl KkStatus {
    fn new(panel: &Entity<KkPanel>, cx: &mut Context<Self>) -> Self {
        let mut previous_errors = (None, None);
        let subscription = cx.observe(panel, move |_, panel, cx| {
            let state = panel.read(cx);
            let errors = (state.error.clone(), state.operation_error.clone());
            let mut messages = Vec::new();
            if errors.0 != previous_errors.0
                && let Some(error) = &errors.0
            {
                messages.push(error.clone());
            }

            if errors.1 != previous_errors.1
                && let Some((_, error)) = &errors.1
            {
                state.output.read(cx).diagnostics().error(error);
                messages.push(error.clone());
            }

            previous_errors = errors;
            if !messages.is_empty() {
                let workspace = state.workspace.clone();
                let panel = panel.downgrade();
                let message = messages.join("\n");
                // Workspace may be updating the panel when this observer runs.
                cx.defer(move |cx| {
                    if let Some(workspace) = workspace.upgrade() {
                        let toast = Self::error_toast(panel, message, cx);
                        workspace.update(cx, |workspace, cx| {
                            workspace.toggle_status_toast(toast, cx);
                        });
                    }
                });
            }
            cx.notify();
        });
        Self {
            panel: panel.downgrade(),
            _subscription: subscription,
        }
    }

    fn error_toast(
        panel: WeakEntity<KkPanel>,
        message: String,
        cx: &mut App,
    ) -> Entity<StatusToast> {
        let summary: String = message
            .lines()
            .next()
            .unwrap_or("KK operation failed")
            .chars()
            .take(100)
            .collect();
        StatusToast::new(format!("KK: {summary}"), cx, |toast, _| {
            toast
                .icon(
                    Icon::new(IconName::XCircle)
                        .size(IconSize::Small)
                        .color(Color::Error),
                )
                .dismiss_button(true)
                .auto_dismiss(false)
                .action("View Output", move |window, cx| {
                    if let Some(panel) = panel.upgrade() {
                        panel.update(cx, |panel, cx| panel.show_output(window, cx));
                    }
                })
        })
    }
}

impl Render for KkStatus {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let progress = self.panel.upgrade().and_then(|panel| {
            let panel = panel.read(cx);
            if panel.pending_writes > 0 {
                Some(panel.pending_status.to_owned())
            } else if let Some((_, label, _)) = &panel.opening_feedback {
                Some(label.clone())
            } else if panel.loading {
                Some("Loading…".to_owned())
            } else {
                None
            }
        });
        div().when_some(progress, |view, message| {
            view.child(
                h_flex()
                    .id("kk-progress-status")
                    .gap_1()
                    .max_w(rems(24.))
                    .overflow_hidden()
                    .child(ui::SpinnerLabel::new())
                    .child(Label::new(format!("KK: {message}")).truncate())
                    .tooltip(Tooltip::text(message)),
            )
        })
    }
}

impl workspace::StatusItemView for KkStatus {
    fn hide_setting(&self, _: &App) -> Option<workspace::HideStatusItem> {
        None
    }

    fn set_active_pane_item(
        &mut self,
        _: Option<&dyn workspace::item::ItemHandle>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) {
    }
}

pub struct KkPanel {
    workspace: WeakEntity<Workspace>,
    output: Entity<KkOutput>,
    focus: FocusHandle,
    fs: std::sync::Arc<dyn project::Fs>,
    repositories: Vec<RepositoryView>,
    repository_paths: HashMap<PathBuf, Vec<(EntityId, PathBuf)>>,
    repository_lifetimes: HashMap<PathBuf, kk_workbench::RepositoryLifetime>,
    mode: HistoryMode,
    pending_history_mode: Option<HistoryMode>,
    history_preference: HistoryPreference,
    history_limits: HashMap<PathBuf, usize>,
    history_loads: HashMap<PathBuf, HistoryLoad>,
    updating_stale: HashSet<PathBuf>,
    loading: bool,
    refresh_queued: bool,
    refresh_demand: RefreshDemand,
    error: Option<String>,
    refresh_task: Task<()>,
    refresh_sequence: u64,
    refreshing_repositories: HashSet<PathBuf>,
    refresh_errors: HashMap<PathBuf, Vec<String>>,
    discovery_errors: HashMap<PathBuf, Vec<String>>,
    load_tasks: HashMap<String, Task<()>>,
    model_generation: u64,
    buffer_subscriptions: HashMap<EntityId, Vec<Subscription>>,
    scroll_handle: UniformListScrollHandle,
    selected_change: Option<(PathBuf, String, String)>,
    reveal_selected: bool,
    pending_picker: Option<Purpose>,
    pending_reveal: Option<(PathBuf, Revision)>,
    comparison_state: ComparisonState,
    file_order: FileOrdering,
    pub(super) diff_display: Entity<super::kk_settings::DiffDisplayPreference>,
    pub(super) commit_information: Entity<CommitInformation>,
    tree_selection: TreeSelectionState,
    tree_cache: HashMap<PathBuf, std::sync::Arc<tree::RepositoryTree>>,
    tree_revision: u64,
    tree_requested: Option<u64>,
    tree_task: Task<()>,
    visible_files: Vec<FileRow>,
    file_batches: HashMap<FileBatchKey, Task<()>>,
    classification_batch_sequence: u64,
    file_opening: FileOpeningState,
    opening_feedback: Option<(u64, String, HashSet<tree::TreeRowId>)>,
    opening_sequence: u64,
    context_menu: Option<(Entity<ContextMenu>, Point<Pixels>, Subscription)>,
    pending_writes: usize,
    pending_status: &'static str,
    operation_error: Option<(PathBuf, String)>,
    uncertain_operations: HashMap<PathBuf, OperationReceipt>,
    write_queues: HashMap<PathBuf, RepositoryQueue>,
    list_previews: ListPreviews,
    mutation_previews: MutationPreviews,
    _subscriptions: Vec<Subscription>,
}

pub fn init(cx: &mut App) {
    let select_all = if cx.platform_info().os == gpui::OperatingSystem::Mac {
        "cmd-a"
    } else {
        "ctrl-a"
    };
    cx.bind_keys([
        gpui::KeyBinding::new(select_all, editor::actions::SelectAll, Some("KKWorkbench")),
        gpui::KeyBinding::new("up", menu::SelectPrevious, Some("KKWorkbench")),
        gpui::KeyBinding::new("down", menu::SelectNext, Some("KKWorkbench")),
        gpui::KeyBinding::new("shift-up", editor::actions::SelectUp, Some("KKWorkbench")),
        gpui::KeyBinding::new(
            "shift-down",
            editor::actions::SelectDown,
            Some("KKWorkbench"),
        ),
        gpui::KeyBinding::new("home", menu::SelectFirst, Some("KKWorkbench")),
        gpui::KeyBinding::new("end", menu::SelectLast, Some("KKWorkbench")),
        gpui::KeyBinding::new(
            "shift-home",
            editor::actions::SelectToBeginning,
            Some("KKWorkbench"),
        ),
        gpui::KeyBinding::new(
            "shift-end",
            editor::actions::SelectToEnd,
            Some("KKWorkbench"),
        ),
        gpui::KeyBinding::new("left", menu::SelectParent, Some("KKWorkbench")),
        gpui::KeyBinding::new("right", menu::SelectChild, Some("KKWorkbench")),
        gpui::KeyBinding::new("enter", menu::Confirm, Some("KKWorkbench")),
    ]);
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_key_context_contributor(|workspace, context, cx| {
            if let Some(panel) = workspace.panel::<KkPanel>(cx) {
                context.add("KKWorkbenchAvailable");
                let panel = panel.read(cx);
                if panel
                    .repositories
                    .iter()
                    .any(|repository| !repository.history.revisions.is_empty())
                {
                    context.add("KKWorkbenchLoaded");
                }

                if panel
                    .repositories
                    .iter()
                    .any(|repository| repository.comparison.is_some())
                {
                    context.add("KKWorkbenchComparison");
                }
            }
        });
        workspace.register_action(|workspace, _: &Refresh, window, cx| {
            if let Some(panel) = workspace.panel::<KkPanel>(cx) {
                window.defer(cx, move |_, cx| {
                    panel.update(cx, |panel, cx| panel.refresh(cx))
                });
            }
        });
        workspace.register_action(|workspace, _: &ShowOutput, window, cx| {
            if let Some(panel) = workspace.panel::<KkPanel>(cx) {
                window.defer(cx, move |window, cx| {
                    panel.update(cx, |panel, cx| panel.show_output(window, cx));
                });
            }
        });
        workspace.register_action(|workspace, _: &CloseComparison, window, cx| {
            if let Some(panel) = workspace.panel::<KkPanel>(cx) {
                window.defer(cx, move |window, cx| {
                    panel.update(cx, |panel, cx| {
                        panel.close_comparison(&CloseComparison, window, cx)
                    })
                });
            }
        });
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<KkPanel>(window, cx);
        });
        workspace.register_action(|workspace, _: &UpdateStaleWorkspace, window, cx| {
            if let Some(panel) = workspace.panel::<KkPanel>(cx) {
                window.defer(cx, move |window, cx| {
                    panel.update(cx, |panel, cx| {
                        panel.update_stale(&UpdateStaleWorkspace, window, cx)
                    });
                });
            }
        });
        workspace.register_action(|workspace, _: &CompareWith, window, cx| {
            if let Some(panel) = workspace.focus_panel::<KkPanel>(window, cx) {
                window.defer(cx, move |window, cx| {
                    panel.update(cx, |panel, cx| {
                        panel.show_picker(None, Purpose::Compare, window, cx)
                    });
                });
            }
        });
        workspace.register_action(|workspace, _: &FindChange, window, cx| {
            if let Some(panel) = workspace.focus_panel::<KkPanel>(window, cx) {
                window.defer(cx, move |window, cx| {
                    panel.update(cx, |panel, cx| {
                        panel.show_picker(None, Purpose::Find, window, cx)
                    });
                });
            }
        });
        workspace.register_action(|workspace, _: &SearchInChanges, window, cx| {
            if let Some(panel) = workspace.panel::<KkPanel>(cx) {
                window.defer(cx, move |window, cx| {
                    panel.update(cx, |panel, cx| panel.search_current(true, window, cx));
                });
            }
        });
        workspace.register_action(|workspace, _: &SearchInRevision, window, cx| {
            if let Some(panel) = workspace.panel::<KkPanel>(cx) {
                window.defer(cx, move |window, cx| {
                    panel.update(cx, |panel, cx| panel.search_current(false, window, cx));
                });
            }
        });
    })
    .detach();
}

impl KkPanel {
    pub(super) fn diagnostics(&self, cx: &App) -> kk_workbench::Diagnostics {
        self.output.read(cx).diagnostics().clone()
    }

    #[cfg(test)]
    pub(super) fn refresh_for_test(&mut self, cx: &mut Context<Self>) -> Task<()> {
        self.start_refresh(cx);
        std::mem::replace(&mut self.refresh_task, Task::ready(()))
    }

    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> anyhow::Result<Entity<Self>> {
        workspace
            .clone()
            .update_in(&mut cx, |workspace_view, window, cx| {
                let project = workspace_view.project().clone();
                let workspace_id = workspace_view
                    .database_id()
                    .map(|identifier| serde_json::to_string(&identifier))
                    .transpose()?;
                let file_order = FileOrdering::new(workspace_id.clone(), cx)?;
                let history_preference = HistoryPreference::new(workspace_id.clone(), cx)?;
                let diff_display =
                    super::kk_settings::DiffDisplayPreference::new(workspace_id.clone(), cx)?;
                let diff_display = cx.new(|_| diff_display);
                let commit_information = CommitInformation::new(workspace_id, cx)?;
                let commit_information = cx.new(|_| commit_information);
                let categories = KkWorkbenchSettings::get_global(cx).debug.clone();
                let output = cx.new(|cx| KkOutput::new(&categories, window, cx));
                let remote = project.read(cx).kk_workbench_client(cx);
                let host = project
                    .read(cx)
                    .remote_connection_options(cx)
                    .map(|options| options.host_key());
                if let Some(remote) = remote {
                    output.update(cx, |output, cx| output.connect(remote, &categories, cx));
                }

                workspace_view.add_panel(output.clone(), window, cx);
                let panel = cx.new(|cx| {
                    let mut panel = Self {
                        workspace,
                        output,
                        focus: cx.focus_handle(),
                        fs: project.read(cx).fs().clone(),
                        repositories: Vec::new(),
                        repository_paths: HashMap::new(),
                        repository_lifetimes: HashMap::new(),
                        mode: history_preference.mode,
                        pending_history_mode: None,
                        history_preference,
                        history_limits: HashMap::new(),
                        history_loads: HashMap::new(),
                        updating_stale: HashSet::new(),
                        loading: false,
                        refresh_queued: false,
                        refresh_demand: RefreshDemand::new(window.window_handle()),
                        error: None,
                        refresh_task: Task::ready(()),
                        refresh_sequence: 0,
                        refreshing_repositories: HashSet::new(),
                        refresh_errors: HashMap::new(),
                        discovery_errors: HashMap::new(),
                        load_tasks: HashMap::new(),
                        model_generation: 0,
                        buffer_subscriptions: HashMap::new(),
                        scroll_handle: UniformListScrollHandle::new(),
                        selected_change: None,
                        reveal_selected: false,
                        pending_picker: None,
                        pending_reveal: None,
                        comparison_state: ComparisonState::new(host, cx),
                        file_order,
                        diff_display,
                        commit_information,
                        tree_selection: TreeSelectionState::default(),
                        tree_cache: HashMap::new(),
                        tree_revision: 0,
                        tree_requested: None,
                        tree_task: Task::ready(()),
                        visible_files: Vec::new(),
                        file_batches: HashMap::new(),
                        classification_batch_sequence: 0,
                        file_opening: FileOpeningState::default(),
                        opening_feedback: None,
                        opening_sequence: 0,
                        context_menu: None,
                        pending_writes: 0,
                        pending_status: "",
                        operation_error: None,
                        uncertain_operations: HashMap::new(),
                        write_queues: HashMap::new(),
                        list_previews: ListPreviews::default(),
                        mutation_previews: MutationPreviews::default(),
                        _subscriptions: Vec::new(),
                    };
                    panel.subscribe(project, window, cx);
                    panel
                });
                let status = cx.new(|cx| KkStatus::new(&panel, cx));
                workspace_view.status_bar().update(cx, |bar, cx| {
                    bar.add_right_item(status, window, cx);
                });
                cx.observe(&panel, |_, _, cx| cx.notify()).detach();
                Ok(panel)
            })?
    }

    fn subscribe(
        &mut self,
        project: Entity<project::Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self._subscriptions.push(cx.observe_self(|this, _| {
            this.tree_revision = this.tree_revision.wrapping_add(1);
        }));
        let store = project.read(cx).buffer_store().clone();
        if let Some(remote) = project.read(cx).remote_client() {
            let mut connected = remote.read(cx).connection().is_some();
            self._subscriptions
                .push(cx.observe(&remote, move |this, remote, cx| {
                    let next = remote.read(cx).connection().is_some();
                    if next && !connected {
                        this.model_generation += 1;
                        this.invalidate_refresh(cx);
                    }

                    connected = next;
                }));
        }

        for buffer in store.read(cx).buffers().collect::<Vec<_>>() {
            self.subscribe_buffer(buffer, cx);
        }

        self._subscriptions
            .push(cx.subscribe(&store, |this, _, event, cx| {
                if let project::buffer_store::BufferStoreEvent::BufferAdded(buffer) = event {
                    this.subscribe_buffer(buffer.clone(), cx);
                }
            }));
        self._subscriptions
            .push(cx.subscribe(&project, |this, _, event, cx| {
                if matches!(
                    event,
                    project::Event::WorktreeAdded(_)
                        | project::Event::WorktreeRemoved(_)
                        | project::Event::WorktreePathsChanged { .. }
                ) {
                    this.model_generation += 1;
                    this.invalidate_closed_repositories(cx);
                    this.invalidate_refresh(cx);
                }
            }));
        self._subscriptions
            .push(cx.observe_window_activation(window, |this, window, cx| {
                if window.is_window_active() {
                    this.invalidate_refresh(cx);
                }
            }));
        let mut executable = KkWorkbenchSettings::get_global(cx).executable_path.clone();
        self._subscriptions
            .push(cx.observe_global::<SettingsStore>(move |this, cx| {
                let categories = KkWorkbenchSettings::get_global(cx).debug.clone();
                this.output
                    .update(cx, |output, cx| output.configure(&categories, cx));
                let next = KkWorkbenchSettings::get_global(cx).executable_path.clone();
                if executable != next {
                    executable = next;
                    this.model_generation += 1;
                    this.invalidate_refresh(cx);
                }
            }));
        self._subscriptions
            .push(cx.on_focus_in(&self.focus.clone(), window, |this, _, cx| {
                this.schedule_focused_refresh(cx)
            }));
        if let Some(workspace) = self.workspace.upgrade() {
            self._subscriptions
                .push(cx.subscribe(&workspace, |this, _, event, cx| match event {
                    workspace::Event::TerminalFocusLost => this.invalidate_refresh(cx),
                    workspace::Event::ItemAdded { .. }
                    | workspace::Event::ActiveItemChanged
                    | workspace::Event::PaneAdded(_)
                    | workspace::Event::PaneRemoved => this.schedule_focused_refresh(cx),
                    _ => {}
                }));
        }
        self.schedule_focused_refresh(cx);
    }

    fn subscribe_buffer(&mut self, buffer: Entity<language::Buffer>, cx: &mut Context<Self>) {
        let buffer_id = buffer.entity_id();
        let subscription = cx.subscribe(&buffer, |this, buffer, event, cx| {
            if matches!(event, language::BufferEvent::Saved) {
                if let Some(file) = buffer.read(cx).file().cloned()
                    && let Some(path) = file.file_system_abs_path(cx) {
                    let canonical = canonical_local_path(file.as_ref(), &path);
                    for root in this.repository_lifetimes.keys() {
                        if let Ok(relative) = canonical.as_deref().unwrap_or(path.as_path()).strip_prefix(root)
                            && let Some(relative) = relative.to_str() {
                            if let Some(queue) = this.write_queues.get_mut(root) {
                                queue.flush();
                            }

                            if file.is_local() && let Err(error) = kk_workbench::record_saved_file(root, relative) {
                                this.operation_error = Some((root.to_owned(), format!("The file was saved, but its pending classification could not record the new version: {error:#}")));
                                cx.notify();
                            }
                        }
                    }
                }
            }

            if matches!(event, language::BufferEvent::Saved) {
                this.invalidate_refresh(cx);
            }
        });
        let release = cx.observe_release(&buffer, move |this, _, _| {
            this.buffer_subscriptions.remove(&buffer_id);
        });
        self.buffer_subscriptions
            .insert(buffer_id, vec![subscription, release]);
    }

    pub(super) fn reveal_revision(
        &mut self,
        root: PathBuf,
        revision: Revision,
        cx: &mut Context<Self>,
    ) {
        self.pending_reveal = None;
        let Some(repository) = self
            .repositories
            .iter_mut()
            .find(|repository| repository.snapshot.repository().root() == root)
        else {
            self.pending_reveal = Some((root, revision));
            if !self.loading {
                self.refresh(cx);
            }
            cx.notify();
            return;
        };
        self.selected_change = Some((
            root,
            revision.full_change_id.clone(),
            revision.full_commit_id.clone(),
        ));
        repository
            .expanded
            .entry(revision.full_commit_id.clone())
            .or_insert(RevisionFiles::Loading);
        if !repository
            .history_targets
            .iter()
            .any(|target| target.full_commit_id == revision.full_commit_id)
        {
            repository.history_targets.push(revision);
        }
        self.reveal_selected = true;
        self.model_generation += 1;
        self.refresh(cx);
    }

    pub(super) fn set_file_order(&mut self, order: FileOrder, cx: &mut Context<Self>) {
        self.file_order.set(order, cx, Self::show_error);
    }

    pub(super) fn is_for_workspace(&self, workspace: &gpui::WeakEntity<Workspace>) -> bool {
        &self.workspace == workspace
    }

    pub(super) fn file_order(&self) -> &FileOrdering {
        &self.file_order
    }

    fn show_picker(
        &mut self,
        repository_index: Option<usize>,
        purpose: Purpose,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.repositories.is_empty() && self.loading {
            self.pending_picker = Some(purpose);
            return;
        }

        let Some(repository_index) = repository_index else {
            self.choose_repository(window, cx, move |this, index, window, cx| {
                this.show_picker(Some(index), purpose, window, cx)
            });
            return;
        };

        let Some(repository) = self.repositories.get(repository_index) else {
            self.error = Some("No KK repositories found in this project".into());
            cx.notify();
            return;
        };
        let snapshot = repository.snapshot.clone();

        let panel = cx.weak_entity();
        self.workspace
            .update(cx, |workspace, cx| {
                workspace.toggle_modal(window, cx, |window, cx| {
                    kk_revision_picker::new(
                        snapshot,
                        purpose,
                        window,
                        cx,
                        move |root, revision, window, cx| {
                            panel
                                .update(cx, |panel, cx| {
                                    let Some(index) =
                                        panel.repositories.iter().position(|repository| {
                                            repository.snapshot.repository().root() == root
                                        })
                                    else {
                                        return;
                                    };
                                    if purpose == Purpose::Compare {
                                        panel.load_files(index, revision, true, cx);
                                    } else {
                                        window.focus(&panel.focus, cx);
                                        panel.reveal_revision(root, revision, cx);
                                    }
                                })
                                .log_err();
                        },
                    )
                });
            })
            .log_err();
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.loading = true;
        if !self.refresh_queued {
            self.refresh_queued = true;
            let panel = cx.weak_entity();
            cx.defer(move |cx| {
                panel
                    .update(cx, |this, cx| {
                        this.refresh_queued = false;
                        this.start_refresh(cx);
                    })
                    .log_err();
            });
        }

        cx.notify();
    }

    fn start_refresh(&mut self, cx: &mut Context<Self>) {
        self.refresh_demand.invalidate();
        self.start_refresh_with_scope(true, cx);
    }

    fn start_refresh_with_scope(&mut self, refresh_open_diffs: bool, cx: &mut Context<Self>) {
        let refresh_version = self.refresh_demand.version;
        self.refresh_demand.requested = false;
        self.refresh_demand.running_version = refresh_version;
        self.refresh_demand.refresh_open_diffs = refresh_open_diffs;
        let Some(workspace) = self.workspace.upgrade() else {
            self.loading = false;
            self.pending_history_mode = None;
            return;
        };
        let project = workspace.read(cx).project().read(cx);
        let remote = project.kk_workbench_client(cx);
        if !project.is_local() && remote.is_none() {
            self.loading = false;
            self.pending_history_mode = None;
            self.error =
                Some("KK Workbench requires a local project or a remote host connection".into());
            cx.notify();
            return;
        }

        let paths: Vec<(EntityId, PathBuf)> = project
            .visible_worktrees(cx)
            .map(|tree| {
                let identifier = tree.entity_id();
                let tree = tree.read(cx);
                let path = tree.abs_path();
                let path = if tree.is_single_file() {
                    path.parent().unwrap_or(&path).to_owned()
                } else {
                    path.to_path_buf()
                };
                (identifier, path)
            })
            .collect();
        let mode = self.mode;
        let (comparison_store, pending_writes, comparison_host) = self.comparison_state.reader();
        let diagnostics = self.output.read(cx).diagnostics().clone();
        let performance = diagnostics.start("history.refresh", None, serde_json::Value::Null);
        let request = RefreshRequest {
            paths,
            remote,
            executable: KkWorkbenchSettings::get_global(cx).executable_path.clone(),
            previous: self
                .repositories
                .iter()
                .filter(|view| self.repository_is_open(view.snapshot.repository().root(), cx))
                .cloned()
                .collect(),
            mode,
            history_limits: self.history_limits.clone(),
            uncertain: self.uncertain_operations.clone(),
            comparison_store,
            comparison_host,
            diagnostics: diagnostics.clone(),
            performance_context: performance.context().cloned(),
        };
        let generation = self.model_generation;
        self.refresh_sequence += 1;
        let sequence = self.refresh_sequence;
        let confirmed_list_previews = self.list_previews.confirmed();
        let confirmed_mutations = self.mutation_previews.confirmed();
        self.loading = true;
        self.refreshing_repositories = self
            .repositories
            .iter()
            .map(|view| view.snapshot.repository().root().to_owned())
            .collect();
        let active_paths = request.paths.clone();
        self.refresh_task = cx.spawn(async move |this, cx| {
            pending_writes.await;
            let (sender, mut events) = futures::channel::mpsc::unbounded();
            let worker = cx.background_spawn(request.run(sender));
            let mut completed = HashSet::new();
            while let Some(event) = events.next().await {
                let accepted = this
                    .update(cx, |this, cx| {
                        if this.refresh_sequence != sequence {
                            return false;
                        }
                        if this.model_generation != generation {
                            if refresh_open_diffs {
                                this.refresh(cx);
                            } else {
                                this.loading = false;
                                this.refreshing_repositories.clear();
                                this.schedule_focused_refresh(cx);
                            }
                            return false;
                        }

                        match event {
                            RefreshEvent::Discovered {
                                worktree,
                                path,
                                roots,
                                errors,
                            } => {
                                // Failed discovery cannot establish that cached repositories disappeared.
                                if errors.is_empty() {
                                    for paths in this.repository_paths.values_mut() {
                                        paths.retain(|(identifier, previous)| {
                                            *identifier != worktree || previous != &path
                                        });
                                    }
                                    this.discovery_errors.remove(&path);
                                } else {
                                    for error in &errors {
                                        diagnostics.error(error);
                                    }
                                    this.discovery_errors.insert(path.clone(), errors);
                                }
                                for root in roots {
                                    if !completed.contains(&root) {
                                        this.refreshing_repositories.insert(root.clone());
                                    }
                                    let paths = this.repository_paths.entry(root).or_default();
                                    if !paths.contains(&(worktree, path.clone())) {
                                        paths.push((worktree, path.clone()));
                                    }
                                    paths.sort_by_key(|attachment| {
                                        active_paths
                                            .iter()
                                            .position(|active| active == attachment)
                                            .unwrap_or(usize::MAX)
                                    });
                                }
                            }
                            RefreshEvent::Loaded(completion) => {
                                let root = completion.root.clone();
                                completed.insert(root.clone());
                                this.refreshing_repositories.remove(&root);
                                if !this.repository_is_open(&root, cx) {
                                    return true;
                                }

                                let errors = this.apply_repository_refresh(
                                    *completion,
                                    &confirmed_list_previews,
                                    &confirmed_mutations,
                                    cx,
                                );
                                if errors.is_empty() {
                                    this.refresh_errors.remove(&root);
                                } else {
                                    this.refresh_errors.insert(root, errors);
                                }
                            }
                        }
                        this.update_refresh_errors();
                        cx.notify();
                        true
                    })
                    .log_err();
                if accepted != Some(true) {
                    performance.finish("superseded", serde_json::Value::Null);
                    return;
                }
            }
            worker.await;
            this.update(cx, |this, cx| {
                if this.refresh_sequence != sequence {
                    return;
                }
                if this.model_generation != generation {
                    if refresh_open_diffs {
                        this.refresh(cx);
                    } else {
                        this.loading = false;
                        this.refreshing_repositories.clear();
                        this.schedule_focused_refresh(cx);
                    }
                    return;
                }

                this.loading = false;
                this.refreshing_repositories.clear();
                this.pending_history_mode = None;
                for paths in this.repository_paths.values_mut() {
                    paths.retain(|attachment| active_paths.contains(attachment));
                }
                this.discovery_errors
                    .retain(|path, _| active_paths.iter().any(|(_, active)| active == path));
                let open = this
                    .repository_paths
                    .iter()
                    .filter(|(_, paths)| !paths.is_empty())
                    .map(|(root, _)| root.clone())
                    .collect::<HashSet<_>>();
                this.repositories
                    .retain(|view| open.contains(view.snapshot.repository().root()));
                this.invalidate_closed_repositories(cx);
                this.repository_paths.retain(|_, paths| !paths.is_empty());
                this.tree_selection.update_roots(&this.repositories);
                this.refresh_errors.retain(|root, _| open.contains(root));
                this.update_refresh_errors();
                if let Some((root, revision)) = this.pending_reveal.take() {
                    if this
                        .repositories
                        .iter()
                        .any(|repository| repository.snapshot.repository().root() == root)
                    {
                        this.reveal_revision(root, revision, cx);
                    } else {
                        this.show_error(
                            "The repository for this diff is no longer open".into(),
                            cx,
                        );
                    }
                }
                let errors = this
                    .refresh_errors
                    .values()
                    .chain(this.discovery_errors.values())
                    .map(Vec::len)
                    .sum::<usize>();
                if errors == 0 {
                    this.refresh_demand.list_version = refresh_version;
                }
                if errors == 0
                    || this.refresh_demand.version != refresh_version
                    || this.refresh_demand.requested
                {
                    this.schedule_focused_refresh(cx);
                }
                performance.finish(
                    if errors == 0 { "completed" } else { "error" },
                    serde_json::json!({"repositories": this.repositories.len(), "errors": errors}),
                );
                cx.notify();
            })
            .log_err();
        });
        cx.notify();
    }

    fn update_refresh_errors(&mut self) {
        let mut errors = self
            .refresh_errors
            .values()
            .chain(self.discovery_errors.values())
            .flatten()
            .cloned()
            .collect::<Vec<_>>();
        errors.sort();
        errors.dedup();
        self.error = (!errors.is_empty()).then(|| errors.join("\n"));
    }

    fn apply_repository_refresh(
        &mut self,
        completion: RepositoryRefresh,
        confirmed_list_previews: &HashSet<u64>,
        confirmed_mutations: &HashSet<u64>,
        cx: &mut Context<Self>,
    ) -> Vec<String> {
        let RepositoryRefresh {
            root,
            view,
            mut errors,
            settled,
            loaded_comparisons,
        } = completion;
        let (mut views, history_errors) = match view {
            Ok(mut view) => {
                view.snapshot = view.snapshot.with_performance_context(None);
                (vec![view], Vec::new())
            }
            Err(error) => {
                errors.push(format!("{}: {error:#}", root.display()));
                if let Some(previous) = self
                    .repositories
                    .iter_mut()
                    .find(|view| view.snapshot.repository().root() == root)
                {
                    previous.history_targets.retain(|target| {
                        previous
                            .history
                            .revisions
                            .iter()
                            .any(|revision| revision.full_commit_id == target.full_commit_id)
                    });
                }
                (Vec::new(), vec![root])
            }
        };
        for error in &errors {
            self.output.read(cx).diagnostics().error(error);
        }
        if self.reveal_selected
            && self
                .selected_change
                .as_ref()
                .is_some_and(|(root, _, _)| history_errors.contains(root))
        {
            self.reveal_selected = false;
            self.selected_change = self.selected_tree_revision().and_then(|(index, revision)| {
                Some((
                    self.repositories
                        .get(index)?
                        .snapshot
                        .repository()
                        .root()
                        .to_owned(),
                    revision.full_change_id,
                    revision.full_commit_id,
                ))
            });
        }

        self.history_loads.retain(|root, state| {
            if history_errors.contains(root) {
                *state = HistoryLoad::Failed;
                true
            } else {
                !views
                    .iter()
                    .any(|view| view.snapshot.repository().root() == root)
            }
        });
        for view in &mut views {
            let root = view.snapshot.repository().root().to_owned();
            let lifetime = self.repository_lifetimes.entry(root).or_default().clone();
            view.snapshot = view.snapshot.with_lifetime(Some(lifetime));
        }

        for (root, applied, snapshot) in settled {
            self.list_previews
                .settle_uncertain(&root, applied, loaded_comparisons.contains(&root));
            let mutations_refreshed = views.iter().any(|view| {
                view.snapshot.repository().root() == root
                    && (view.comparison.is_none() || loaded_comparisons.contains(&root))
                    && view
                        .expanded
                        .values()
                        .all(|files| matches!(files, RevisionFiles::Loaded(_)))
            });
            self.mutation_previews
                .settle_uncertain(&root, applied, mutations_refreshed);
            self.uncertain_operations.remove(&root);
            if let Some(workspace) = self.workspace.upgrade() {
                let diffs = workspace
                    .read(cx)
                    .items_of_type::<KkDiff>(cx)
                    .collect::<Vec<_>>();
                for diff in diffs {
                    diff.update(cx, |diff, cx| {
                        diff.resolve_classification_uncertainty(&snapshot, applied, cx)
                    });
                }
                let multi_diffs = workspace
                    .read(cx)
                    .items_of_type::<KkMultiDiff>(cx)
                    .collect::<Vec<_>>();
                for diff in multi_diffs {
                    diff.update(cx, |diff, cx| {
                        diff.resolve_classification_uncertainty(&snapshot, applied, cx)
                    });
                }
            }

            if let Some(resumed) = self
                .write_queues
                .get_mut(&root)
                .and_then(|queue| queue.resume(applied, &snapshot))
            {
                for preview in resumed.rejected_previews {
                    preview
                        .model
                        .update(cx, |model, _| model.remove(preview.id))
                        .log_err();
                }

                if resumed.run {
                    self.run_write_queue(root.clone(), cx);
                }
            }

            if self
                .operation_error
                .as_ref()
                .is_some_and(|(path, _)| *path == root)
            {
                self.operation_error = if applied {
                    None
                } else {
                    Some((root, "The previous operation was not published. Select the changes again to retry.".into()))
                };
            }
        }

        for view in &views {
            let limit = self
                .history_limits
                .entry(view.snapshot.repository().root().to_owned())
                .or_insert(50);
            *limit = (*limit).max(view.history.revisions.len());
        }

        self.list_previews
            .refreshed(confirmed_list_previews, &views, &loaded_comparisons);
        self.mutation_previews
            .refreshed(confirmed_mutations, &views, &loaded_comparisons);
        let changed = views.iter().any(|view| {
            self.repositories
                .iter()
                .find(|previous| {
                    previous.snapshot.repository().root() == view.snapshot.repository().root()
                })
                .is_none_or(|previous| !previous.same_content(view))
        });
        for view in &views {
            if let Some(previous) = self.repositories.iter_mut().find(|previous| {
                previous.snapshot.repository().root() == view.snapshot.repository().root()
            }) {
                *previous = view.clone();
            } else {
                let index = self
                    .repositories
                    .iter()
                    .position(|previous| {
                        previous.snapshot.repository().root() > view.snapshot.repository().root()
                    })
                    .unwrap_or(self.repositories.len());
                self.repositories.insert(index, view.clone());
            }
        }
        self.tree_selection.update_roots(&self.repositories);
        if changed {
            for view in &views {
                self.refresh_repository_emptiness(Some(view.snapshot.repository().root()), cx);
            }
        }
        for view in &views {
            if !view.stale {
                self.refresh_demand.snapshots.insert(
                    view.snapshot.repository().root().to_owned(),
                    (self.refresh_demand.running_version, view.snapshot.clone()),
                );
            }
        }
        if self.refresh_demand.refresh_open_diffs
            && let Some(workspace) = self.workspace.upgrade()
        {
            let diffs = workspace
                .read(cx)
                .items_of_type::<KkDiff>(cx)
                .collect::<Vec<_>>();
            let multi_diffs = workspace
                .read(cx)
                .items_of_type::<KkMultiDiff>(cx)
                .collect::<Vec<_>>();
            let blame = workspace
                .read(cx)
                .status_bar()
                .read(cx)
                .item_of_type::<super::kk_blame::KkBlameStatus>();
            for repository in &views {
                if repository.stale {
                    continue;
                }

                if let Some(blame) = &blame {
                    blame.update(cx, |blame, cx| blame.refresh(&repository.snapshot, cx));
                }

                for diff in &diffs {
                    diff.update(cx, |diff, cx| diff.refresh(repository.snapshot.clone(), cx));
                }
                for diff in &multi_diffs {
                    diff.update(cx, |diff, cx| diff.refresh(&repository.snapshot, cx));
                }
            }
        }

        for repository in &views {
            if let Some(comparison) = &repository.comparison {
                self.comparison_state.save(
                    repository.snapshot.repository().root(),
                    Some(&comparison.intent),
                    cx,
                    Self::show_error,
                );
            }
        }
        if self.pending_reveal.as_ref().is_some_and(|(root, _)| {
            views
                .iter()
                .any(|view| view.snapshot.repository().root() == root)
        }) && let Some((root, revision)) = self.pending_reveal.take()
        {
            if self
                .repositories
                .iter()
                .any(|repository| repository.snapshot.repository().root() == root)
            {
                self.reveal_revision(root, revision, cx);
            } else {
                self.pending_reveal = Some((root, revision));
            }
        }
        cx.notify();
        errors
    }

    fn load_files(
        &mut self,
        repository_index: usize,
        revision: Revision,
        comparison: bool,
        cx: &mut Context<Self>,
    ) {
        self.model_generation += 1;
        let Some(repository) = self.repositories.get_mut(repository_index) else {
            return;
        };
        let snapshot = repository.snapshot.clone();
        let root = snapshot.repository().root().to_owned();
        let operation = snapshot.operation().to_owned();
        self.selected_change = Some((
            root.clone(),
            revision.full_change_id.clone(),
            revision.full_commit_id.clone(),
        ));
        let key = if comparison {
            format!("compare:{}", root.display())
        } else {
            format!("commit:{}:{}", root.display(), revision.full_commit_id)
        };
        if comparison {
            let comparison = Comparison::new(&revision);
            self.comparison_state.save(
                repository.snapshot.repository().root(),
                Some(&comparison.intent),
                cx,
                Self::show_error,
            );
            repository.comparison = Some(comparison);
        } else {
            if repository
                .expanded
                .remove(&revision.full_commit_id)
                .is_some()
            {
                self.load_tasks.remove(&key);
                cx.notify();
                return;
            }

            repository
                .expanded
                .insert(revision.full_commit_id.clone(), RevisionFiles::Loading);
        }

        let intent = repository
            .comparison
            .as_ref()
            .map(|comparison| comparison.intent.clone());
        let task_key = key.clone();
        let task = cx.spawn(async move |this, cx| {
            let revision_for_read = revision.clone();
            let result = cx
                .background_spawn(async move {
                    if comparison {
                        let intent =
                            intent.ok_or_else(|| anyhow::anyhow!("Comparison was closed"))?;
                        Ok((Vec::new(), Some(Comparison::load(&snapshot, intent).await?)))
                    } else {
                        anyhow::Ok((
                            snapshot
                                .changed_files(&DiffScope::Change(revision_for_read.full_commit_id))
                                .await?,
                            None,
                        ))
                    }
                })
                .await;
            this.update(cx, |this, cx| {
                this.load_tasks.remove(&task_key);
                let Some(repository) = this.repositories.iter_mut().find(|repository| {
                    repository.snapshot.repository().root() == root
                        && repository.snapshot.operation() == operation
                }) else {
                    return;
                };
                match result {
                    Ok((_, Some(comparison))) => {
                        this.comparison_state.save(
                            &root,
                            Some(&comparison.intent),
                            cx,
                            Self::show_error,
                        );
                        repository.comparison = Some(comparison);
                    }
                    Ok((files, None)) => {
                        repository
                            .expanded
                            .insert(revision.full_commit_id, RevisionFiles::Loaded(files));
                    }
                    Err(error) if !comparison => {
                        repository.expanded.insert(
                            revision.full_commit_id,
                            RevisionFiles::Failed(format!("{error:#}")),
                        );
                    }
                    Err(error) => this.error = Some(format!("{error:#}")),
                }

                this.model_generation += 1;
                cx.notify();
            })
            .log_err();
        });
        self.load_tasks.insert(key, task);
        cx.notify();
    }

    fn show_error(&mut self, error: String, cx: &mut Context<Self>) {
        self.output.read(cx).diagnostics().error(&error);
        self.error = Some(error);
        cx.notify();
    }

    fn show_output(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = self.workspace.clone();
        // Revealing another dock reads the active panels, including this one.
        window.defer(cx, move |window, cx| {
            if let Some(workspace) = workspace.upgrade() {
                workspace.update(cx, |workspace, cx| {
                    workspace.reveal_panel::<KkOutput>(window, cx)
                });
            }
        });
    }

    fn selected_revision(&self) -> Option<(usize, Revision)> {
        if self.tree_selection.focused_root().is_some() {
            return self.selected_tree_revision();
        }

        let (root, change_id, commit_id) = self.selected_change.as_ref()?;
        let (index, repository) = self
            .repositories
            .iter()
            .enumerate()
            .find(|(_, repository)| repository.snapshot.repository().root() == root)?;
        let revisions = || repository.history.revisions.iter();
        let revision = revisions()
            .find(|revision| revision.full_commit_id == *commit_id)
            .or_else(|| {
                let mut matching =
                    revisions().filter(|revision| revision.full_change_id == *change_id);
                let revision = matching.next()?;
                matching.next().is_none().then_some(revision)
            })?;
        Some((index, revision.clone()))
    }

    fn copy_revision_id(&self, commit: bool, cx: &mut App) {
        if let Some((_, revision)) = self.selected_revision() {
            cx.write_to_clipboard(ClipboardItem::new_string(if commit {
                revision.full_commit_id
            } else {
                revision.full_change_id
            }));
        }
    }

    fn retry_commit(
        &mut self,
        repository_index: usize,
        revision: Revision,
        cx: &mut Context<Self>,
    ) {
        if let Some(repository) = self.repositories.get_mut(repository_index) {
            repository.expanded.remove(&revision.full_commit_id);
        }

        self.load_files(repository_index, revision, false, cx);
    }

    fn revision_menu(
        &mut self,
        repository_index: usize,
        revision: Revision,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(repository) = self.repositories.get(repository_index) else {
            return;
        };
        let root = repository.snapshot.repository().root().to_owned();
        self.selected_change = Some((
            root.clone(),
            revision.full_change_id.clone(),
            revision.full_commit_id.clone(),
        ));
        let failed = matches!(
            repository.expanded.get(&revision.full_commit_id),
            Some(RevisionFiles::Failed(_))
        );
        let panel = cx.weak_entity();
        let menu = ContextMenu::build(window, cx, |menu, _, _| {
            menu.entry("KKW: Open Changes", None, {
                let panel = panel.clone();
                let root = root.clone();
                let revision = revision.clone();
                move |window, cx| {
                    panel
                        .update(cx, |panel, cx| {
                            if let Some(index) = panel.repositories.iter().position(|repository| {
                                repository.snapshot.repository().root() == root
                            }) {
                                panel.open_all_changes(index, Some(revision.clone()), window, cx);
                            }
                        })
                        .log_err();
                }
            })
            .separator()
            .when(!revision.is_working_copy, |menu| {
                let root = root.clone();
                let revision = revision.clone();
                let panel = panel.clone();
                menu.entry("KKW: Compare with Change", None, move |window, cx| {
                    panel
                        .update(cx, |panel, cx| {
                            if let Some(index) = panel.repositories.iter().position(|repository| {
                                repository.snapshot.repository().root() == root
                            }) {
                                panel.load_files(index, revision.clone(), true, cx);
                                window.focus(&panel.focus, cx);
                            }
                        })
                        .log_err();
                })
                .separator()
            })
            .entry("KKW: Copy Change ID", None, {
                let identifier = revision.full_change_id.clone();
                move |_, cx| cx.write_to_clipboard(ClipboardItem::new_string(identifier.clone()))
            })
            .entry("KKW: Copy Commit ID", None, {
                let identifier = revision.full_commit_id.clone();
                move |_, cx| cx.write_to_clipboard(ClipboardItem::new_string(identifier.clone()))
            })
            .when(failed, |menu| {
                menu.entry("KKW: Retry Loading Commit", None, move |_, cx| {
                    panel
                        .update(cx, |panel, cx| {
                            if let Some(index) = panel.repositories.iter().position(|repository| {
                                repository.snapshot.repository().root() == root
                            }) {
                                panel.retry_commit(index, revision.clone(), cx);
                            }
                        })
                        .log_err();
                })
            })
        });
        self.show_context_menu(menu, position, window, cx);
    }

    fn show_context_menu(
        &mut self,
        menu: Entity<ContextMenu>,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let previous_focus = window.focused(cx);
        window.focus(&menu.focus_handle(cx), cx);
        let subscription = cx.subscribe_in(
            &menu,
            window,
            move |this, menu, _: &DismissEvent, window, cx| {
                if menu.focus_handle(cx).contains_focused(window, cx)
                    && let Some(previous_focus) = previous_focus.as_ref()
                {
                    window.focus(previous_focus, cx);
                }
                this.context_menu.take();
                cx.notify();
            },
        );
        self.context_menu = Some((menu, position, subscription));
        cx.notify();
    }

    fn choose_repository(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        action: impl FnOnce(&mut Self, usize, &mut Window, &mut Context<Self>) + 'static,
    ) {
        if let Some(index) = self.tree_selection.focused_root().and_then(|root| {
            self.repositories
                .iter()
                .position(|repository| repository.snapshot.repository().root() == root)
        }) {
            action(self, index, window, cx);
            return;
        }

        let roots = self
            .repositories
            .iter()
            .map(|repository| repository.snapshot.repository().root().to_owned())
            .collect::<Vec<_>>();
        let selected = super::picker_prompt::prompt(
            "Select a KK repository",
            roots
                .iter()
                .map(|root| SharedString::from(root.display().to_string()))
                .collect(),
            self.workspace.clone(),
            window,
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            let Some(root) = selected.await.and_then(|index| roots.get(index)) else {
                return;
            };
            this.update_in(cx, |this, window, cx| {
                if let Some(index) = this
                    .repositories
                    .iter()
                    .position(|repository| repository.snapshot.repository().root() == root)
                {
                    action(this, index, window, cx);
                }
            })
            .log_err();
        })
        .detach();
    }

    fn search_current(&mut self, comparison: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.choose_repository(window, cx, move |this, index, window, cx| {
            this.search_selected_repository(index, comparison, window, cx)
        });
    }

    fn search_selected_repository(
        &mut self,
        index: usize,
        comparison: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let revision = if comparison {
            None
        } else {
            self.selected_revision()
                .filter(|(selected, _)| *selected == index)
                .map(|(_, revision)| revision)
                .or_else(|| {
                    self.repositories
                        .get(index)?
                        .history
                        .revisions
                        .iter()
                        .find(|revision| revision.is_working_copy)
                        .cloned()
                })
        };
        self.search_revision(index, revision, window, cx);
    }

    fn search_revision(
        &mut self,
        index: usize,
        revision: Option<Revision>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(repository) = self.repositories.get(index) else {
            return;
        };
        let target = if let Some(revision) = revision {
            super::kk_diff::DiffTarget::change(&revision)
        } else if let Some(comparison) = &repository.comparison {
            comparison.diff_target()
        } else {
            self.show_error(
                "Choose a comparison before searching its changes".into(),
                cx,
            );
            return;
        };
        if let Some(workspace) = self.workspace.upgrade() {
            DiffSearch::open(
                repository.snapshot.clone(),
                target,
                None,
                workspace,
                window,
                cx,
            );
        }
    }

    fn close_comparison(
        &mut self,
        _: &CloseComparison,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.choose_repository(window, cx, |this, index, _, cx| {
            this.close_repository_comparison(index, cx)
        });
    }

    fn close_repository_comparison(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(repository) = self.repositories.get_mut(index) else {
            return;
        };
        self.model_generation += 1;
        self.load_tasks.remove(&format!(
            "compare:{}",
            repository.snapshot.repository().root().display()
        ));
        repository.comparison = None;
        self.comparison_state.save(
            repository.snapshot.repository().root(),
            None,
            cx,
            Self::show_error,
        );
        cx.notify();
    }

    fn set_mode(&mut self, mode: HistoryMode, cx: &mut Context<Self>) {
        if self.mode == mode
            && self
                .repositories
                .iter()
                .all(|repository| repository.mode == mode)
        {
            return;
        }

        self.model_generation += 1;
        self.mode = mode;
        self.pending_history_mode = Some(mode);
        self.history_preference.set(mode, cx, Self::show_error);
        self.refresh(cx);
    }

    fn load_more_commits(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(repository) = self.repositories.get(index) else {
            return;
        };
        let root = repository.snapshot.repository().root().to_owned();
        if !repository.history.has_more
            || self.history_loads.get(&root) == Some(&HistoryLoad::Loading)
        {
            return;
        }

        if self.history_loads.get(&root) != Some(&HistoryLoad::Failed) {
            let limit = self.history_limits.entry(root.clone()).or_insert(50);
            *limit = (*limit)
                .max(repository.history.revisions.len())
                .saturating_add(50);
        }

        self.history_loads.insert(root, HistoryLoad::Loading);
        self.model_generation += 1;
        self.refresh(cx);
    }

    fn open_file(
        &mut self,
        repository_index: usize,
        mut target: super::kk_diff::DiffTarget,
        file: ChangedFile,
        working_copy: bool,
        mode: DiffOpenMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(repository) = self.repositories.get(repository_index) else {
            return;
        };
        let snapshot = repository.snapshot.clone();
        self.error = None;
        cx.notify();
        if file.has_conflict {
            self.open_conflict(snapshot, target, file.path, window, cx);
            return;
        }

        if let Some(comparison) = &repository.comparison
            && matches!(&target.scope, DiffScope::Change(commit) if comparison.sources.contains(commit))
        {
            target.comparison = Some(comparison.intent.clone());
        }

        let rows = self
            .tree_selection
            .opening_rows(snapshot.repository().root(), &file.path);
        let label = format!("Opening diff: {}…", file.path);
        let open = self.open_diff_task(snapshot, target, file, working_copy, mode, window, cx);
        let open = self.track_opening(label, Some(rows), open, cx);
        let task = cx.spawn_in(window, async move |this, cx| {
            if let Err(error) = open.await {
                this.update(cx, |this, cx| {
                    this.error = Some(format!("{error:#}"));
                    cx.notify();
                })
                .log_err();
            }
        });
        self.load_tasks.insert("open-diff".into(), task);
    }

    fn open_conflict(
        &mut self,
        snapshot: Snapshot,
        target: super::kk_diff::DiffTarget,
        path: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let snapshot = match self.bind_snapshot(snapshot, cx) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.show_error(format!("{error:#}"), cx);
                return;
            }
        };
        let opening = self.file_opening.begin(&snapshot);
        let revision = match target.scope {
            DiffScope::Change(revision) => revision,
            DiffScope::Comparison { target, .. } => target,
        };
        let rows = self
            .tree_selection
            .opening_rows(snapshot.repository().root(), &path);
        let label = format!("Opening conflict: {path}…");
        let key = format!(
            "conflict:{}:{revision}:{path}",
            snapshot.repository().root().display()
        );
        let task = super::kk_conflict::KkConflict::open(
            snapshot,
            revision,
            path,
            cx.weak_entity(),
            self.workspace.clone(),
            window,
            cx,
        );
        let task = cx.spawn(async move |_, _| opening.wait(task).await);
        let task = self.track_opening(label, Some(rows), task, cx);
        self.load_tasks.insert(
            key,
            cx.spawn(async move |this, cx| {
                if let Err(error) = task.await {
                    this.update(cx, |this, cx| {
                        this.error = Some(format!("{error:#}"));
                        cx.notify();
                    })
                    .log_err();
                }
            }),
        );
    }

    pub(super) fn open_plain_file(
        &mut self,
        snapshot: Snapshot,
        target: super::kk_diff::DiffTarget,
        file: ChangedFile,
        working_copy: bool,
        kind: FileOpenKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.error = None;
        cx.notify();
        let rows = self
            .tree_selection
            .opening_rows(snapshot.repository().root(), &file.path);
        let label = format!("Opening file: {}…", file.path);
        let task =
            self.open_plain_file_task(snapshot, target, file, working_copy, kind, window, cx);
        let task = self.track_opening(label, Some(rows), task, cx);
        self.load_tasks.insert(
            "open-diff".into(),
            cx.spawn(async move |this, cx| {
                if let Err(error) = task.await {
                    this.update(cx, |this, cx| {
                        Self::show_error(this, format!("{error:#}"), cx)
                    })
                    .log_err();
                }
            }),
        );
    }

    fn open_plain_file_task(
        &mut self,
        snapshot: Snapshot,
        target: super::kk_diff::DiffTarget,
        file: ChangedFile,
        working_copy: bool,
        kind: FileOpenKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        let snapshot = match self.bind_snapshot(snapshot, cx) {
            Ok(snapshot) => snapshot,
            Err(error) => return Task::ready(Err(error)),
        };
        let opening = self.file_opening.begin(&snapshot);
        let paths = if !file.has_conflict
            && (kind == FileOpenKind::Current || kind == FileOpenKind::Revision && working_copy)
        {
            vec![file.path.clone()]
        } else {
            Vec::new()
        };
        let preparation = self.prepare_open(snapshot, paths, &opening, window, cx);
        cx.spawn_in(window, async move |this, cx| {
            let preparation = preparation.await;
            if !opening.is_current() {
                return Ok(());
            }

            let Some(snapshot) = preparation.map_err(|error| anyhow::anyhow!(error.to_string()))?
            else {
                return Ok(());
            };

            let open = this.update_in(cx, |this, window, cx| {
                this.open_prepared_file_task(
                    snapshot,
                    target,
                    file,
                    working_copy,
                    kind,
                    opening.clone(),
                    window,
                    cx,
                )
            })?;
            opening.wait(open).await?;
            Ok(())
        })
    }

    fn open_prepared_file_task(
        &mut self,
        snapshot: Snapshot,
        target: super::kk_diff::DiffTarget,
        file: ChangedFile,
        working_copy: bool,
        kind: FileOpenKind,
        opening: OpeningToken,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        if !opening.is_current() {
            return Task::ready(Ok(()));
        }

        if file.has_conflict && kind != FileOpenKind::Before {
            let revision = match target.scope {
                DiffScope::Change(revision)
                | DiffScope::Comparison {
                    target: revision, ..
                } => revision,
            };
            return super::kk_conflict::KkConflict::open(
                snapshot,
                revision,
                file.path,
                cx.weak_entity(),
                self.workspace.clone(),
                window,
                cx,
            );
        }

        let Some(workspace) = self.workspace.upgrade() else {
            return Task::ready(Err(anyhow::anyhow!("Workspace was closed")));
        };
        let before = kind == FileOpenKind::Before;
        let empty = before
            && matches!(
                file.change_type,
                kk_workbench::ChangeKind::Added | kk_workbench::ChangeKind::Copied
            );
        let revision = match (before, target.scope) {
            (true, DiffScope::Change(_)) => format!("parents({})", target.identity),
            (false, DiffScope::Change(_)) => target.identity,
            (true, DiffScope::Comparison { base, .. }) => base,
            (false, DiffScope::Comparison { target, .. }) => target,
        };
        let path = if before && !empty {
            file.renamed_from.unwrap_or(file.path)
        } else {
            file.path
        };
        let prefer_current =
            kind == FileOpenKind::Current || (kind == FileOpenKind::Revision && working_copy);
        cx.spawn_in(window, async move |_, cx| {
                let (snapshot, current_path) = if prefer_current {
                    let revision = revision.clone();
                    let path = path.clone();
                    cx.background_spawn(async move {
                        let snapshot = snapshot.repository().snapshot(true).await?;
                        let current = snapshot.current_file_path(&revision, &path).await?;
                        anyhow::Ok((snapshot, current))
                    })
                    .await?
                } else {
                    (snapshot, None)
                };
                if !opening.is_current() {
                    return Ok(());
                }

                if let Some(current_path) = current_path {
                    let absolute = snapshot.repository().root().join(&current_path);
                    let open = workspace
                        .update_in(cx, |workspace, window, cx| {
                            if current_path != path {
                                workspace.show_toast(
                                    workspace::Toast::new(
                                        workspace::notifications::NotificationId::unique::<
                                            FileOpenKind,
                                        >(),
                                        format!(
                                            "{path} is now {current_path} in the working copy."
                                        ),
                                    ),
                                    cx,
                                );
                            }

                            let project_path = workspace
                                .project()
                                .read(cx)
                                .find_project_path(&absolute, cx);
                            if let Some(path) = project_path {
                                workspace.open_path(path, None, true, window, cx)
                            } else {
                                workspace.open_abs_path(
                                    absolute,
                                    workspace::OpenOptions::default(),
                                    window,
                                    cx,
                                )
                            }
                        })?;
                    opening.wait(open).await?;
                } else {
                    if prefer_current {
                        workspace.update(cx, |workspace, cx| workspace.show_toast(workspace::Toast::new(workspace::notifications::NotificationId::unique::<FileOpenKind>(), format!("{path} no longer exists in the working copy. Showing read-only historical content.")), cx));
                    }

                    let status = workspace.read_with(cx, |workspace, cx| {
                        workspace
                            .status_bar()
                            .read(cx)
                            .item_of_type::<super::kk_blame::KkBlameStatus>()
                    });
                    let status = status.ok_or_else(|| anyhow::anyhow!("Historical file view is unavailable"))?;
                    let open = status.update_in(cx, |status, window, cx| {
                        status.open_file_task(snapshot, revision, path, empty, window, cx)
                    })?;
                    opening.wait(open).await?;
                }

                anyhow::Ok(())
        })
    }

    fn classify_file(
        &mut self,
        snapshot: Snapshot,
        target: super::kk_diff::DiffTarget,
        intent: ComparisonIntent,
        file: ChangedFile,
        category: ClassificationTarget,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        self.classify_files(
            snapshot,
            target,
            intent,
            vec![FileSelection { file, ranges: None }],
            category,
            cx,
        )
    }

    pub(super) fn classify_files(
        &mut self,
        snapshot: Snapshot,
        target: super::kk_diff::DiffTarget,
        intent: ComparisonIntent,
        files: Vec<FileSelection>,
        category: ClassificationTarget,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        let task = self.classify_files_result(snapshot, target, intent, files, category, cx);
        cx.spawn(async move |_, _| {
            if let Err(error) = task.await {
                log::debug!("KK classification failed: {}", error.message);
            }
        })
    }

    fn classify_files_result(
        &mut self,
        snapshot: Snapshot,
        target: super::kk_diff::DiffTarget,
        intent: ComparisonIntent,
        files: Vec<FileSelection>,
        category: ClassificationTarget,
        cx: &mut Context<Self>,
    ) -> Task<Result<Snapshot, ClassificationFailure>> {
        let mut previews = Vec::new();
        let mut views = Vec::new();
        if let Some(direction) = self.classification_direction(&snapshot, &target, category)
            && let Some(workspace) = self.workspace.upgrade()
        {
            let opened: Vec<_> = workspace.read(cx).items_of_type::<KkDiff>(cx).collect();
            for file in files.iter().filter(|file| file.ranges.is_none()) {
                for view in &opened {
                    if !view
                        .read(cx)
                        .matches_classification(&snapshot, &target, &file.file)
                    {
                        continue;
                    }

                    match view.update(cx, |view, cx| view.preview_whole_file(direction, cx)) {
                        Ok(preview) => {
                            views.push((view.downgrade(), preview.id));
                            previews.push(preview);
                        }
                        Err(error) => {
                            let message = format!("{error:#}");
                            for (view, id) in views {
                                view.update(cx, |view, cx| {
                                    view.finish_classification_preview(
                                        id,
                                        Err(ClassificationFailure {
                                            message: message.clone(),
                                            uncertain: false,
                                        }),
                                        cx,
                                    )
                                })
                                .log_err();
                            }

                            self.show_error(message.clone(), cx);
                            return Task::ready(Err(ClassificationFailure {
                                message,
                                uncertain: false,
                            }));
                        }
                    }
                }
            }
        }

        let task =
            self.queue_classification(snapshot, target, intent, files, category, previews, cx);
        cx.spawn(async move |_, cx| {
            let result = task.await;
            for (view, id) in views {
                view.update(cx, |view, cx| {
                    view.finish_classification_preview(id, result.clone(), cx)
                })
                .log_err();
            }

            result
        })
    }

    pub(super) fn classification_direction(
        &self,
        snapshot: &Snapshot,
        source: &super::kk_diff::DiffTarget,
        target: ClassificationTarget,
    ) -> Option<kk_workbench::TransferDirection> {
        use kk_workbench::TransferDirection;
        if matches!(source.scope, DiffScope::Comparison { .. }) {
            return Some(TransferDirection::Backward);
        }

        let comparison = self
            .repositories
            .iter()
            .find(|repository| {
                repository.snapshot.repository().root() == snapshot.repository().root()
            })?
            .comparison
            .as_ref()?;
        let source_index = comparison
            .categories
            .iter()
            .position(|entry| entry.revision.revset() == source.identity);
        let Some(source_index) = source_index else {
            return Some(TransferDirection::Backward);
        };
        let destination_index = match target {
            ClassificationTarget::Comparison => return Some(TransferDirection::Forward),
            ClassificationTarget::Category(category) => comparison
                .categories
                .iter()
                .position(|entry| entry.category == category),
        };
        Some(
            if destination_index.is_some_and(|destination| destination > source_index) {
                TransferDirection::Forward
            } else {
                TransferDirection::Backward
            },
        )
    }

    pub(super) fn queue_classification(
        &mut self,
        snapshot: Snapshot,
        target: super::kk_diff::DiffTarget,
        intent: ComparisonIntent,
        files: Vec<FileSelection>,
        category: ClassificationTarget,
        previews: Vec<QueuedPreview>,
        cx: &mut Context<Self>,
    ) -> Task<Result<Snapshot, ClassificationFailure>> {
        self.queue_classification_from(
            snapshot, target, intent, files, category, previews, None, None, true, cx,
        )
    }

    fn queue_classification_from(
        &mut self,
        snapshot: Snapshot,
        target: super::kk_diff::DiffTarget,
        intent: ComparisonIntent,
        files: Vec<FileSelection>,
        category: ClassificationTarget,
        previews: Vec<QueuedPreview>,
        source_category: Option<ClassificationTarget>,
        source_dependency: Option<WriteDependency>,
        capture_saved: bool,
        cx: &mut Context<Self>,
    ) -> Task<Result<Snapshot, ClassificationFailure>> {
        let snapshot = match self.bind_write_snapshot(snapshot, cx) {
            Ok(snapshot) => snapshot,
            Err(error) => return Task::ready(Err(error)),
        };
        if self
            .uncertain_operations
            .contains_key(snapshot.repository().root())
        {
            self.show_error("Refresh KK Workbench to confirm the previous operation before classifying more changes".into(), cx);
            return Task::ready(Err(ClassificationFailure {
                message: "The previous operation has not been confirmed".into(),
                uncertain: false,
            }));
        }

        if files
            .iter()
            .any(|selection| self.file_is_dirty(snapshot.repository().root(), &selection.file, cx))
        {
            self.show_error(
                "Save the selected file's drafts before classifying its changes".into(),
                cx,
            );
            return Task::ready(Err(ClassificationFailure {
                message: "Save the selected file's drafts before classifying its changes".into(),
                uncertain: false,
            }));
        }

        let root = snapshot.repository().root().to_owned();
        let (completion, completed) = futures::channel::oneshot::channel();
        // A mutation may replace the working file. Its acknowledged tree is the source,
        // rather than a saved-file capture of the contents before that mutation.
        let job = if !capture_saved {
            Ok(ClassificationJob::without_saved(
                snapshot.clone(),
                target.clone(),
                intent.clone(),
                files.clone(),
                category,
                completion,
                previews,
            ))
        } else {
            ClassificationJob::new(
                snapshot.clone(),
                target.clone(),
                intent.clone(),
                files.clone(),
                category,
                completion,
                previews,
            )
        };
        let mut job = match job {
            Ok(job) => job,
            Err(error) => {
                let message = format!("{error:#}");
                self.show_error(message.clone(), cx);
                return Task::ready(Err(ClassificationFailure {
                    message,
                    uncertain: false,
                }));
            }
        };
        job.source_category = source_category;
        job.source_dependency = source_dependency;
        job.list_previews.push(self.list_previews.push_from(
            snapshot,
            &intent,
            target,
            files,
            category,
            source_category,
            capture_saved,
        ));
        self.pending_writes += 1;
        self.pending_status = "Splitting out…";
        self.operation_error = None;
        cx.notify();
        let start = self
            .write_queues
            .entry(root.clone())
            .or_insert_with(|| RepositoryQueue::new(cx.background_executor().clone()))
            .enqueue(job);
        if start {
            self.run_write_queue(root, cx);
        }

        cx.spawn(async move |_, _| {
            completed.await.unwrap_or_else(|_| {
                Err(ClassificationFailure {
                    message: "KK Workbench was closed before classification completed".into(),
                    uncertain: true,
                })
            })
        })
    }

    fn run_write_queue(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        self.clear_commit_emptiness(&root);
        cx.spawn(async move |this, cx| {
                cx.background_executor().timer(std::time::Duration::ZERO).await;
                loop {
                    let batch = this.update(cx, |this, _| this.write_queues.get_mut(&root).and_then(RepositoryQueue::next));
                    let Ok(Some((mut queued, snapshot))) = batch else { break };
                    let parent = queued.waiting.as_ref().and_then(|waiting| waiting.context()).cloned();
                    if let Some(waiting) = queued.waiting.take() { waiting.finish("completed", serde_json::Value::Null); }
                    let execution = snapshot.repository().diagnostics().start("queue.execute", parent.as_ref(), serde_json::json!({"root": root}));
                    let snapshot = snapshot.with_performance_context(execution.context().cloned());
                    let sparse_result = if let RepositoryJob::Sparse(job) = &queued.job {
                        let result = cx.background_spawn({
                            let snapshot = snapshot.clone();
                            let paths = job.paths.clone();
                            let current = job.current.clone();
                            async move { snapshot.repository().expand_sparse_checkout(&paths, || current()).await }
                        }).await;
                        if matches!(result, Ok(kk_workbench::SparseCheckout::PendingSavedInputs)) {
                            this.update(cx, |this, _| {
                                if let Some(queue) = this.write_queues.get_mut(&root) {
                                    queue.defer_sparse(queued);
                                }
                            }).log_err();
                            cx.background_executor().timer(std::time::Duration::from_millis(16)).await;
                            execution.finish("deferred", serde_json::Value::Null);
                            continue;
                        }

                        Some(result)
                    } else {
                        None
                    };
                    let dependency = queued.check_dependencies();
                    let advance_comparison = queued.has_dependencies();
                    let successes = queued.successes;
                    let previous = snapshot.clone();
                    let retain_uncertain = matches!(&queued.job, RepositoryJob::Conflict(_) | RepositoryJob::Edit(_));
                    let mut list_previews = Vec::new();
                    let mutation_previews = match &queued.job {
                        RepositoryJob::File(job) => job.preview_id.into_iter().collect::<Vec<_>>(),
                        RepositoryJob::Conflict(job) => job.preview_id.into_iter().collect(),
                        _ => Vec::new(),
                    };
                    let (result, original, mut completions, previews) = match queued.job {
                        RepositoryJob::UpdateStale(job) => {
                            let original = job.snapshot.clone();
                            let result = async {
                                this.read_with(cx, |this, cx| this.check_repository_open(&root, cx))??;
                                let snapshot = cx.background_spawn(async move { job.snapshot.repository().update_stale_workspace().await }).await?;
                                anyhow::Ok(ClassificationCompletion { snapshot, publication: None, published_snapshot: None })
                            }.await;
                            (result, original, vec![job.completion], Vec::new())
                        }
                        RepositoryJob::Sparse(job) => {
                            let original = job.snapshot.clone();
                            let result = sparse_result.unwrap_or_else(|| Err(anyhow::anyhow!("Sparse checkout was not evaluated"))).and_then(|result| match result {
                                kk_workbench::SparseCheckout::Ready(snapshot) => Ok(ClassificationCompletion { snapshot, publication: None, published_snapshot: None }),
                                kk_workbench::SparseCheckout::Cancelled => Ok(ClassificationCompletion { snapshot, publication: None, published_snapshot: None }),
                                kk_workbench::SparseCheckout::PendingSavedInputs => Err(anyhow::anyhow!("Sparse checkout still has pending saved inputs")),
                            });
                            this.update(cx, |this, cx| {
                                this.model_generation += 1;
                                this.refresh(cx);
                            })
                            .log_err();
                            (result, original, vec![job.completion], Vec::new())
                        }
                        RepositoryJob::Classification(mut job) => {
                            list_previews = job.list_previews.clone();
                            let allowed = this.read_with(cx, |this, cx| {
                                dependency?;
                                this.check_repository_open(&root, cx)?;
                                anyhow::ensure!(!this.uncertain_operations.contains_key(&root), "Refresh KK Workbench to confirm the previous operation before classifying more changes");
                                job.current_files(cx)
                            });
                            let original = job.snapshot.clone();
                            let completions = std::mem::take(&mut job.completions);
                            let previews = std::mem::take(&mut job.previews);
                            let result = match allowed {
                                Ok(Ok(files)) => cx.background_spawn(job.execute_queued(snapshot, files)).await,
                                Ok(Err(error)) | Err(error) => cx.background_spawn(ClassificationJob::finish(snapshot, job.saved, Err(error))).await,
                            };
                            (result, original, completions, previews)
                        }
                        RepositoryJob::File(job) => {
                            let mut request = job.request.clone();
                            request.snapshot = request.snapshot.with_performance_context(execution.context().cloned());
                            let original = request.snapshot.clone();
                            let result = async {
                                dependency?;
                                this.update(cx, |this, cx| {
                                    this.check_repository_open(&root, cx)?;
                                    anyhow::ensure!(!this.uncertain_operations.contains_key(&root), "Refresh KK Workbench to confirm the previous operation before changing files");
                                    anyhow::ensure!(!this.file_is_dirty(&root, &request.file, cx), "Your draft was retained. Save or discard it before retrying.");
                                    this.pending_status = request.action.label();
                                    cx.notify();
                                    anyhow::Ok(())
                                })??;
                                let (snapshot, prepared) = cx.background_spawn({
                                    let mut request = request.clone();
                                    async move {
                                        if advance_comparison { request.advance(snapshot).await?; }
                                        request.prepare(advance_comparison).await
                                    }
                                }).await?;
                                let publication = if let Some(prepared) = prepared {
                                    this.read_with(cx, |this, cx| {
                                        this.check_repository_open(&root, cx)?;
                                        anyhow::ensure!(!this.file_is_dirty(&root, &request.file, cx), "A new draft was created while the operation was being prepared. The draft was retained and nothing was published.");
                                        anyhow::Ok(())
                                    })??;
                                    Some(cx.background_spawn(prepared.publish()).await?)
                                } else {
                                    None
                                };
                                let published_snapshot = publication.as_ref().map(|publication| publication.snapshot.clone());
                                anyhow::Ok(ClassificationCompletion { snapshot: published_snapshot.clone().unwrap_or(snapshot), publication, published_snapshot })
                            }.await;
                            (result, original, job.take_completions(), Vec::new())
                        }
                        RepositoryJob::Edit(job) => {
                            let original = job.snapshot.clone();
                            let result = async {
                                dependency?;
                                this.read_with(cx, |this, cx| this.check_conflict_write(&root, cx))??;
                                let prepared = cx.background_spawn(async move {
                                    snapshot.repository().snapshot_before_write().await?.prepare_file_edit(&job.edit).await
                                }).await?;
                                this.read_with(cx, |this, cx| this.check_conflict_write(&root, cx))??;
                                let publication = cx.background_spawn(prepared.publish()).await?;
                                let snapshot = publication.snapshot.clone();
                                anyhow::Ok(ClassificationCompletion { snapshot: snapshot.clone(), publication: Some(publication), published_snapshot: Some(snapshot) })
                            }.await;
                            (result, original, vec![job.completion], Vec::new())
                        }
                        RepositoryJob::Conflict(mut job) => {
                            job.request.snapshot = job.request.snapshot.with_performance_context(execution.context().cloned());
                            let original = job.request.snapshot.clone();
                            let result = async {
                                dependency?;
                                this.read_with(cx, |this, cx| this.check_conflict_write(&root, cx))??;
                                let prepared = cx.background_spawn(job.request.prepare(snapshot)).await?;
                                this.read_with(cx, |this, cx| this.check_conflict_write(&root, cx))??;
                                let publication = cx.background_spawn(prepared.publish()).await?;
                                let snapshot = publication.snapshot.clone();
                                anyhow::Ok(ClassificationCompletion { snapshot: snapshot.clone(), publication: Some(publication), published_snapshot: Some(snapshot) })
                            }.await;
                            (result, original, vec![job.completion], Vec::new())
                        }
                    };
                    let outcome = match &result {
                        Ok(result) if result.publication.as_ref().is_none_or(|publication| publication.synchronization_error.is_none()) => Ok(result.snapshot.clone()),
                        Ok(_) => Err(ClassificationFailure { message: "The operation was published but has not been synchronized. Refresh KK Workbench.".into(), uncertain: true }),
                        Err(error) => Err(ClassificationFailure { message: format!("{error:#}"), uncertain: error.downcast_ref::<UncertainPublication>().is_some() }),
                    };
                    let uncertain = outcome.as_ref().is_err_and(|failure| failure.uncertain);
                    execution.finish(if uncertain { "uncertain" } else if outcome.is_ok() { "completed" } else { "error" }, serde_json::Value::Null);
                    let completion_count = completions.len();
                    this.update(cx, |this, cx| {
                        this.list_previews.complete(&list_previews, &outcome);
                        this.mutation_previews.complete(&mutation_previews, &outcome);
                        if outcome.as_ref().is_err_and(|error| !error.uncertain) {
                            for preview in &previews {
                                preview.model.update(cx, |model, _| model.remove(preview.id)).log_err();
                            }
                        }

                        if let Some(queue) = this.write_queues.get_mut(&root) {
                            if uncertain {
                                let mut snapshots = vec![previous.clone(), original.clone()];
                                match &result {
                                    Ok(result) => {
                                        snapshots.push(result.snapshot.clone());
                                        if let Some(publication) = &result.publication {
                                            snapshots.push(publication.receipt.prepared.clone());
                                        }
                                    }
                                    Err(error) => if let Some(uncertain) = error.downcast_ref::<UncertainPublication>() {
                                        snapshots.push(uncertain.receipt.prepared.clone());
                                    },
                                }

                                queue.suspend(successes, snapshots, previews.clone());
                                if retain_uncertain {
                                    queue.retain_conflict_waiters(&mut completions);
                                }
                            } else {
                                queue.complete(&successes, outcome.is_ok());
                            }
                        }

                        this.pending_writes = this.pending_writes.saturating_sub(completion_count.saturating_sub(1));
                        match result {
                            Ok(result) => {
                                if outcome.is_ok() && let Some(queue) = this.write_queues.get_mut(&root) {
                                    queue.advance(&previous, &original, &result.snapshot);
                                    if let Some(publication) = &result.publication {
                                        queue.advance(&publication.receipt.prepared, &original, &result.snapshot);
                                    }

                                    if let Some(published) = &result.published_snapshot {
                                        queue.advance(published, &original, &result.snapshot);
                                    }
                                }

                                if let Some(publication) = result.publication {
                                    this.finish_write(root.clone(), Ok(publication), cx);
                                } else {
                                    this.pending_writes = this.pending_writes.saturating_sub(1);
                                    if !list_previews.is_empty() || !mutation_previews.is_empty() {
                                        this.model_generation += 1;
                                        this.refresh(cx);
                                    }
                                    cx.notify();
                                }
                            }
                            Err(error) => this.finish_write(root.clone(), Err(error), cx),
                        }
                    }).log_err();
                    for completion in completions {
                        // Closing a view may drop its waiter; the repository operation still completes.
                        if completion.send(outcome.clone()).is_err() {
                            log::debug!("KK classification waiter was closed");
                        }
                    }

                    if uncertain {
                        break;
                    }
                }
            }).detach();
    }

    fn finish_write(
        &mut self,
        root: PathBuf,
        result: anyhow::Result<Publication>,
        cx: &mut Context<Self>,
    ) {
        self.pending_writes = self.pending_writes.saturating_sub(1);
        match result {
            Ok(result) => {
                self.diagnostics(cx).log(format!(
                    "Published operation {} in {}",
                    result.snapshot.operation(),
                    root.display()
                ));
                if let Some(error) = result.synchronization_error {
                    self.diagnostics(cx).error(format!(
                        "Published operation could not synchronize {}: {error}",
                        root.display()
                    ));
                    self.uncertain_operations
                        .insert(root.clone(), result.receipt);
                    self.operation_error = Some((
                        root,
                        format!(
                            "The operation was published, but the workspace could not be synchronized: {error}"
                        ),
                    ));
                }

                self.model_generation += 1;
                self.refresh(cx);
            }
            Err(error) => {
                self.diagnostics(cx)
                    .error(format!("{}: {error:#}", root.display()));
                if let Some(uncertain) = error.downcast_ref::<UncertainPublication>() {
                    self.uncertain_operations
                        .insert(root.clone(), uncertain.receipt.clone());
                }

                self.operation_error =
                    Some((root.clone(), format!("{}: {error:#}", root.display())));
                if error.downcast_ref::<UncertainPublication>().is_none() {
                    self.model_generation += 1;
                    self.refresh(cx);
                }
            }
        }

        cx.notify();
    }

    fn file_is_dirty(&self, root: &Path, file: &ChangedFile, cx: &App) -> bool {
        self.workspace.upgrade().is_some_and(|workspace| {
            workspace
                .read(cx)
                .project()
                .read(cx)
                .opened_buffers(cx)
                .into_iter()
                .any(|buffer| {
                    let buffer = buffer.read(cx);
                    buffer.is_dirty()
                        && buffer
                            .file()
                            .and_then(|file| file.file_system_abs_path(cx).map(|path| (file, path)))
                            .is_some_and(|(buffer_file, path)| {
                                let matches = |path: &Path| {
                                    path == root.join(&file.path)
                                        || file.change_type == kk_workbench::ChangeKind::Renamed
                                            && file
                                                .renamed_from
                                                .as_ref()
                                                .is_some_and(|original| path == root.join(original))
                                };
                                matches(path.as_path())
                                    || canonical_local_path(buffer_file.as_ref(), &path)
                                        .is_some_and(|path| matches(&path))
                            })
                })
        })
    }

    fn repository_is_open(&self, root: &Path, cx: &App) -> bool {
        let Some(workspace) = self.workspace.upgrade() else {
            return false;
        };
        workspace
            .read(cx)
            .project()
            .read(cx)
            .visible_worktrees(cx)
            .any(|tree| {
                let identifier = tree.entity_id();
                let tree = tree.read(cx);
                let path = tree.abs_path();
                let path = if tree.is_single_file() {
                    path.parent().unwrap_or(&path)
                } else {
                    &path
                };
                if let Some(attachments) = self.repository_paths.get(root) {
                    attachments
                        .iter()
                        .any(|(worktree, previous)| *worktree == identifier && previous == path)
                } else {
                    root.starts_with(path) || path.starts_with(root)
                }
            })
    }

    fn check_repository_open(&self, root: &Path, cx: &App) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.repository_is_open(root, cx),
            "The selected repository was closed. Reopen it and select the changes again."
        );
        Ok(())
    }

    fn invalidate_closed_repositories(&mut self, cx: &App) {
        let closed = self
            .repository_lifetimes
            .keys()
            .filter(|root| !self.repository_is_open(root, cx))
            .cloned()
            .collect::<Vec<_>>();
        for root in closed {
            self.refresh_demand.snapshots.remove(&root);
            self.tree_cache.remove(&root);
            self.refreshing_repositories.remove(&root);
            self.refresh_errors.remove(&root);
            self.list_previews.forget_root(&root);
            self.mutation_previews.forget_root(&root);
            self.updating_stale.remove(&root);
            self.load_tasks
                .remove(&format!("update-stale:{}", root.display()));
            self.history_limits.remove(&root);
            self.history_loads.remove(&root);
            self.load_tasks.remove(&format!("empty:{}", root.display()));
            self.tree_selection.forget_root(&root);
            if let Some(lifetime) = self.repository_lifetimes.remove(&root) {
                lifetime.invalidate();
            }
        }

        self.file_opening.cancel_closed();
    }

    fn bind_snapshot(&mut self, snapshot: Snapshot, cx: &App) -> anyhow::Result<Snapshot> {
        let repository = snapshot.repository();
        self.check_repository_open(repository.root(), cx)?;
        repository.check_lifetime()?;
        if repository.lifetime().is_some() {
            return Ok(snapshot);
        }

        let lifetime = self
            .repository_lifetimes
            .entry(repository.root().to_owned())
            .or_default()
            .clone();
        Ok(snapshot.with_lifetime(Some(lifetime)))
    }

    fn bind_write_snapshot(
        &mut self,
        snapshot: Snapshot,
        cx: &mut Context<Self>,
    ) -> Result<Snapshot, ClassificationFailure> {
        if self.repositories.iter().any(|repository| {
            repository.stale
                && repository.snapshot.repository().root() == snapshot.repository().root()
        }) {
            let message = "Update the stale workspace before changing its history".to_owned();
            self.show_error(message.clone(), cx);
            return Err(ClassificationFailure {
                message,
                uncertain: false,
            });
        }

        self.bind_snapshot(snapshot, cx).map_err(|error| {
            let message = format!("{error:#}");
            self.show_error(message.clone(), cx);
            ClassificationFailure {
                message,
                uncertain: false,
            }
        })
    }

    fn check_conflict_write(&self, root: &Path, cx: &App) -> anyhow::Result<()> {
        self.check_repository_open(root, cx)?;
        anyhow::ensure!(
            self.repositories
                .iter()
                .any(|repository| repository.snapshot.repository().root() == root),
            "The selected repository was closed. Reopen the repository; your draft is retained."
        );
        anyhow::ensure!(
            !self.uncertain_operations.contains_key(root),
            "Refresh KK Workbench to confirm the previous operation before changing history"
        );
        let workspace = self
            .workspace
            .upgrade()
            .ok_or_else(|| anyhow::anyhow!("Workspace was closed"))?;
        for buffer in workspace.read(cx).project().read(cx).opened_buffers(cx) {
            let buffer = buffer.read(cx);
            if buffer.is_dirty()
                && let Some(file) = buffer.file()
                && let Some(path) = file.file_system_abs_path(cx)
            {
                let canonical = canonical_local_path(file.as_ref(), &path);
                anyhow::ensure!(
                    !path.starts_with(root) && canonical.is_none_or(|path| !path.starts_with(root)),
                    "Save {} before applying, so rewriting history preserves your editor changes.",
                    path.display()
                );
            }
        }

        Ok(())
    }

    pub(super) fn restored_conflict_snapshot(
        &self,
        root: &Path,
        operation: String,
        cx: &App,
    ) -> anyhow::Result<Snapshot> {
        self.check_repository_open(root, cx)?;
        let repository = self
            .repositories
            .iter()
            .find(|view| view.snapshot.repository().root() == root)
            .ok_or_else(|| {
                anyhow::anyhow!("Refresh KK Workbench before applying this restored draft")
            })?;
        repository.snapshot.repository().snapshot_at(operation)
    }

    pub(super) fn save_file_edit(
        &mut self,
        snapshot: Snapshot,
        edit: kk_workbench::FileEdit,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<Snapshot>> {
        let snapshot = match self.bind_write_snapshot(snapshot, cx) {
            Ok(snapshot) => snapshot,
            Err(error) => return Task::ready(Err(anyhow::anyhow!(error.message))),
        };
        let root = snapshot.repository().root().to_owned();
        if let Err(error) = self.check_conflict_write(&root, cx) {
            return Task::ready(Err(error));
        }
        let (completion, completed) = futures::channel::oneshot::channel();
        self.pending_writes += 1;
        self.pending_status = "Saving Diffedit";
        let start = self
            .write_queues
            .entry(root.clone())
            .or_insert_with(|| RepositoryQueue::new(cx.background_executor().clone()))
            .enqueue_edit(FileEditJob {
                snapshot,
                edit,
                completion,
            });
        if start {
            self.run_write_queue(root, cx);
        }
        cx.notify();
        cx.spawn(async move |_, _| {
            completed
                .await
                .map_err(|error| {
                    anyhow::anyhow!(
                        "Diffedit outcome was not confirmed; your draft is retained: {error}"
                    )
                })?
                .map_err(|failure| anyhow::anyhow!(failure.message))
        })
    }

    pub(super) fn resolve_conflict(
        &mut self,
        mut request: ConflictRequest,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<Snapshot>> {
        request.snapshot = match self.bind_write_snapshot(request.snapshot, cx) {
            Ok(snapshot) => snapshot,
            Err(error) => return Task::ready(Err(anyhow::anyhow!(error.message))),
        };
        let root = request.snapshot.repository().root().to_owned();
        if let Err(error) = self.check_conflict_write(&root, cx) {
            self.operation_error = Some((root, format!("{error:#}")));
            cx.notify();
            return Task::ready(Err(error));
        }

        let (completion, completed) = futures::channel::oneshot::channel();
        self.pending_writes += 1;
        self.pending_status = if request.restore.is_some() {
            "Undoing resolution"
        } else {
            "Applying resolution"
        };
        let preview_id = self
            .repositories
            .iter()
            .find(|repository| {
                request.restore.is_none() && repository.snapshot.repository().root() == root
            })
            .map(|repository| self.mutation_previews.conflict(repository, &request));
        let start = self
            .write_queues
            .entry(root.clone())
            .or_insert_with(|| RepositoryQueue::new(cx.background_executor().clone()))
            .enqueue_conflict(ConflictJob {
                request,
                preview_id,
                completion,
            });
        if start {
            self.run_write_queue(root, cx);
        }

        cx.notify();
        cx.spawn(async move |_, _| {
            completed
                .await
                .map_err(|error| {
                    anyhow::anyhow!(
                        "Resolution outcome was not confirmed; your draft is retained: {error}"
                    )
                })?
                .map_err(|failure| anyhow::anyhow!(failure.message))
        })
    }

    fn mutate_file(
        &mut self,
        snapshot: Snapshot,
        target: super::kk_diff::DiffTarget,
        file: ChangedFile,
        action: FileMutationAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        let snapshot = match self.bind_write_snapshot(snapshot, cx) {
            Ok(snapshot) => snapshot,
            Err(_) => return Task::ready(()),
        };
        let root = snapshot.repository().root().to_owned();
        if self.uncertain_operations.contains_key(&root) {
            self.show_error(
                "Refresh KK Workbench to confirm the previous operation before changing files"
                    .into(),
                cx,
            );
            return Task::ready(());
        }

        if self.file_is_dirty(&root, &file, cx) {
            self.show_error(
                "Save or discard the selected file's drafts before moving or reverting it".into(),
                cx,
            );
            return Task::ready(());
        }

        let dependencies = if action == FileMutationAction::Revert
            && matches!(target.scope, DiffScope::Comparison { .. })
        {
            let paths = std::iter::once(file.path.clone())
                .chain(file.renamed_from.clone())
                .collect::<Vec<_>>();
            self.write_queues
                .get(&root)
                .map(|queue| queue.dependencies(&paths, snapshot.repository().lifetime()))
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let prompt = matches!(action, FileMutationAction::Revert).then(|| {
            window.prompt(
                gpui::PromptLevel::Warning,
                &format!(
                    "Revert {}? This will discard its current changes.",
                    file.path
                ),
                None,
                &["Revert File", "Cancel"],
                cx,
            )
        });
        let request = FileMutationRequest {
            snapshot,
            target,
            file,
            action,
        };
        if let Some(prompt) = prompt {
            cx.spawn(async move |this, cx| {
                match prompt.await {
                    Ok(0) => {}
                    Ok(_) => return,
                    Err(error) => {
                        log::debug!("File revert confirmation closed: {error}");
                        return;
                    }
                }

                if let Ok(task) = this.update(cx, |this, cx| {
                    this.queue_file_mutation(request, dependencies, cx)
                }) {
                    task.await;
                }
            })
        } else {
            self.queue_file_mutation(request, dependencies, cx)
        }
    }

    fn queue_file_mutation(
        &mut self,
        request: FileMutationRequest,
        dependencies: Vec<WriteDependency>,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        let task = self.queue_file_mutation_result(request, dependencies, cx);
        cx.spawn(async move |_, _| {
            if let Err(error) = task.await {
                log::debug!("KK file operation failed: {}", error.message);
            }
        })
    }

    fn queue_file_mutation_result(
        &mut self,
        mut request: FileMutationRequest,
        dependencies: Vec<WriteDependency>,
        cx: &mut Context<Self>,
    ) -> Task<Result<Snapshot, ClassificationFailure>> {
        request.snapshot = match self.bind_write_snapshot(request.snapshot, cx) {
            Ok(snapshot) => snapshot,
            Err(error) => return Task::ready(Err(error)),
        };
        let root = request.snapshot.repository().root().to_owned();
        if self.file_is_dirty(&root, &request.file, cx) {
            let message = "Your draft was retained. Save or discard it before retrying.".to_owned();
            self.operation_error = Some((root, message.clone()));
            cx.notify();
            return Task::ready(Err(ClassificationFailure {
                message,
                uncertain: false,
            }));
        }

        let (completion, completed) = futures::channel::oneshot::channel();
        self.pending_writes += 1;
        self.pending_status = request.action.label();
        self.error = None;
        self.operation_error = None;
        let preview_id = self
            .repositories
            .iter()
            .find(|repository| repository.snapshot.repository().root() == root)
            .map(|repository| self.mutation_previews.file(repository, &request));
        let mut job = FileMutationJob::new(request, completion, dependencies);
        job.preview_id = preview_id;
        let start = self
            .write_queues
            .entry(root.clone())
            .or_insert_with(|| RepositoryQueue::new(cx.background_executor().clone()))
            .enqueue_file(job);
        if start {
            self.run_write_queue(root, cx);
        }

        cx.notify();
        cx.spawn(async move |_, _| {
            completed.await.map_err(|error| ClassificationFailure {
                message: format!("File operation outcome was not confirmed: {error}"),
                uncertain: true,
            })?
        })
    }

    fn file_menu(
        &mut self,
        repository_index: usize,
        target: super::kk_diff::DiffTarget,
        file: ChangedFile,
        working_copy: bool,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(repository) = self.repositories.get(repository_index) else {
            return;
        };
        let FileRow {
            snapshot,
            target,
            file,
            working_copy,
            can_move_parent,
            can_move_child,
            category: current_category,
            ..
        } = FileRow::new(repository, target, file, "menu", working_copy);
        let intent = target.comparison.clone();
        let mut mutations = Vec::new();
        if working_copy {
            mutations.push(FileMutationAction::Revert);
        }

        if can_move_parent {
            mutations.push(FileMutationAction::Move(FileMoveDirection::Parent));
        }

        if can_move_child {
            mutations.push(FileMutationAction::Move(FileMoveDirection::Child));
        }

        let panel = cx.entity().downgrade();
        let menu = ContextMenu::build(window, cx, |mut menu, _, _| {
            if file.has_conflict {
                let panel = panel.clone();
                let snapshot = snapshot.clone();
                let target = target.clone();
                let path = file.path.clone();
                menu = menu
                    .entry("KKW: Resolve Conflict", None, move |window, cx| {
                        panel
                            .update(cx, |panel, cx| {
                                panel.open_conflict(
                                    snapshot.clone(),
                                    target.clone(),
                                    path.clone(),
                                    window,
                                    cx,
                                )
                            })
                            .log_err();
                    })
                    .separator();
            }

            for (label, kind) in [
                ("Open File", FileOpenKind::Current),
                ("Blame File", FileOpenKind::Revision),
                ("Open File Before Change", FileOpenKind::Before),
            ] {
                let snapshot = snapshot.clone();
                let target = target.clone();
                let file = file.clone();
                let panel = panel.clone();
                menu = menu.entry(format!("KKW: {label}"), None, move |window, cx| {
                    panel
                        .update(cx, |panel, cx| {
                            panel.open_plain_file(
                                snapshot.clone(),
                                target.clone(),
                                file.clone(),
                                working_copy,
                                kind,
                                window,
                                cx,
                            )
                        })
                        .log_err();
                });
            }

            if !mutations.is_empty() {
                menu = menu.separator();
                for action in mutations {
                    let panel = panel.clone();
                    let snapshot = snapshot.clone();
                    let target = target.clone();
                    let file = file.clone();
                    menu = menu.entry(
                        format!("KKW: {}", action.label()),
                        None,
                        move |window, cx| {
                            panel
                                .update(cx, |panel, cx| {
                                    panel
                                        .mutate_file(
                                            snapshot.clone(),
                                            target.clone(),
                                            file.clone(),
                                            action,
                                            window,
                                            cx,
                                        )
                                        .detach()
                                })
                                .log_err();
                        },
                    );
                }
            }

            if let Some(intent) = intent {
                menu = menu.separator();
                let mut categories = [
                    Category::Pending,
                    Category::Reviewed,
                    Category::Rejected,
                    Category::Debug,
                ]
                .into_iter()
                .filter(|category| Some(*category) != current_category)
                .map(|category| (category.label(), ClassificationTarget::Category(category)))
                .collect::<Vec<_>>();
                if current_category.is_some() {
                    categories.push(("Comparison", ClassificationTarget::Comparison));
                }

                for (label, category) in categories {
                    let panel = panel.clone();
                    let snapshot = snapshot.clone();
                    let target = target.clone();
                    let intent = intent.clone();
                    let file = file.clone();
                    menu = menu.entry(format!("KKW: {label}"), None, move |_, cx| {
                        panel
                            .update(cx, |panel, cx| {
                                panel
                                    .classify_file(
                                        snapshot.clone(),
                                        target.clone(),
                                        intent.clone(),
                                        file.clone(),
                                        category,
                                        cx,
                                    )
                                    .detach()
                            })
                            .log_err();
                    });
                }
            }

            menu
        });
        self.show_context_menu(menu, position, window, cx);
    }
}

impl Drop for KkPanel {
    fn drop(&mut self) {
        for lifetime in self.repository_lifetimes.values() {
            lifetime.invalidate();
        }
    }
}

impl Focusable for KkPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl EventEmitter<PanelEvent> for KkPanel {}

impl Panel for KkPanel {
    fn persistent_name() -> &'static str {
        "KK Workbench"
    }
    fn panel_key() -> &'static str {
        "KKWorkbench"
    }
    fn position(&self, _: &Window, cx: &App) -> DockPosition {
        KkWorkbenchSettings::get_global(cx).dock
    }
    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }
    fn set_position(&mut self, position: DockPosition, _: &mut Window, cx: &mut Context<Self>) {
        settings::update_settings_file(self.fs.clone(), cx, move |settings, _| {
            settings.kk_workbench.get_or_insert_default().dock = Some(position.into());
        });
    }
    fn default_size(&self, _: &Window, _: &App) -> Pixels {
        px(360.)
    }
    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        Some(IconName::GitBranch)
    }
    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("KK Workbench")
    }
    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }
    fn activation_priority(&self) -> u32 {
        4
    }
    fn set_active(&mut self, active: bool, _: &mut Window, cx: &mut Context<Self>) {
        if active {
            self.schedule_focused_refresh(cx);
        }
    }
}

impl Render for KkPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.loading
            && let Some(purpose) = self.pending_picker.take()
        {
            let panel = cx.weak_entity();
            window.defer(cx, move |window, cx| {
                panel
                    .update(cx, |panel, cx| panel.show_picker(None, purpose, window, cx))
                    .log_err();
            });
        }

        let row_count = self.request_tree(window, cx);

        v_flex()
            .id("kk-workbench")
            .key_context("KKWorkbench")
            .track_focus(&self.focus)
            .size_full()
            .on_action(cx.listener(|this, _: &menu::SelectNext, _, cx| {
                this.navigate_tree(1, false, false, cx)
            }))
            .on_action(cx.listener(|this, _: &menu::SelectPrevious, _, cx| {
                this.navigate_tree(-1, false, false, cx)
            }))
            .on_action(cx.listener(|this, _: &menu::SelectFirst, _, cx| {
                this.navigate_tree(-1, true, false, cx)
            }))
            .on_action(cx.listener(|this, _: &menu::SelectLast, _, cx| {
                this.navigate_tree(1, true, false, cx)
            }))
            .on_action(cx.listener(|this, _: &editor::actions::SelectUp, _, cx| {
                this.navigate_tree(-1, false, true, cx)
            }))
            .on_action(cx.listener(|this, _: &editor::actions::SelectDown, _, cx| {
                this.navigate_tree(1, false, true, cx)
            }))
            .on_action(
                cx.listener(|this, _: &editor::actions::SelectToBeginning, _, cx| {
                    this.navigate_tree(-1, true, true, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &editor::actions::SelectToEnd, _, cx| {
                    this.navigate_tree(1, true, true, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &menu::SelectChild, window, cx| {
                this.expand_tree(true, window, cx)
            }))
            .on_action(cx.listener(|this, _: &menu::SelectParent, window, cx| {
                this.expand_tree(false, window, cx)
            }))
            .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| {
                this.confirm_tree(DiffOpenMode::Open, window, cx)
            }))
            .on_action(cx.listener(|this, _: &editor::actions::SelectAll, _, cx| {
                this.select_all_tree_rows(cx)
            }))
            .on_action(cx.listener(|this, _: &Refresh, _, cx| this.refresh(cx)))
            .on_action(cx.listener(Self::update_stale))
            .on_action(cx.listener(Self::close_comparison))
            .on_action(cx.listener(|this, _: &OpenSelectedFiles, window, cx| {
                this.run_selected_files(FileAction::Open(FileOpenKind::Current), window, cx)
            }))
            .on_action(cx.listener(|this, _: &RevertSelectedFiles, window, cx| {
                this.run_selected_files(FileAction::Mutate(FileMutationAction::Revert), window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &MoveSelectedFilesToParent, window, cx| {
                    this.run_selected_files(
                        FileAction::Mutate(FileMutationAction::Move(FileMoveDirection::Parent)),
                        window,
                        cx,
                    )
                }),
            )
            .on_action(
                cx.listener(|this, _: &MoveSelectedFilesToChild, window, cx| {
                    this.run_selected_files(
                        FileAction::Mutate(FileMutationAction::Move(FileMoveDirection::Child)),
                        window,
                        cx,
                    )
                }),
            )
            .on_action(
                cx.listener(|this, _: &CopyChangeId, _, cx| this.copy_revision_id(false, cx)),
            )
            .on_action(cx.listener(|this, _: &CopyCommitId, _, cx| this.copy_revision_id(true, cx)))
            .on_action(cx.listener(|this, _: &RetryCommit, _, cx| {
                if let Some((index, revision)) = this.selected_revision() {
                    this.retry_commit(index, revision, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &SortFilesByName, _, cx| {
                this.file_order
                    .set(FileOrder::FileName, cx, Self::show_error)
            }))
            .on_action(cx.listener(|this, _: &UseRepositoryFileOrder, _, cx| {
                this.file_order
                    .set(FileOrder::Repository, cx, Self::show_error)
            }))
            .on_action(cx.listener(|this, _: &ShowCurrentLineage, _, cx| {
                this.set_mode(HistoryMode::Lineage, cx)
            }))
            .on_action(
                cx.listener(|this, _: &ShowLogHistory, _, cx| this.set_mode(HistoryMode::Log, cx)),
            )
            .on_action(cx.listener(|this, _: &LoadMoreCommits, window, cx| {
                this.choose_repository(window, cx, |this, index, _, cx| {
                    this.load_more_commits(index, cx)
                });
            }))
            .child(
                h_flex()
                    .gap_1()
                    .p_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Label::new("KK Workbench").truncate()),
                    )
                    .when(
                        self.repositories
                            .iter()
                            .any(|repository| !repository.history.revisions.is_empty()),
                        |header| {
                            header
                                .child(
                                    IconButton::new("find-change", IconName::MagnifyingGlass)
                                        .aria_label("Find Change")
                                        .tooltip(Tooltip::for_action_title(
                                            "Find Change",
                                            &FindChange,
                                        ))
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.show_picker(None, Purpose::Find, window, cx)
                                        })),
                                )
                                .child(
                                    IconButton::new(
                                        "history-mode",
                                        if self.mode == HistoryMode::Lineage {
                                            IconName::Crosshair
                                        } else {
                                            IconName::GitBranch
                                        },
                                    )
                                    .aria_label(if self.mode == HistoryMode::Lineage {
                                        "Switch to Default Log"
                                    } else {
                                        "Switch to Current Lineage"
                                    })
                                    .tooltip(if self.mode == HistoryMode::Lineage {
                                        Tooltip::for_action_title(
                                            "Current Lineage — Switch to Default Log",
                                            &ShowLogHistory,
                                        )
                                    } else {
                                        Tooltip::for_action_title(
                                            "Default Log — Switch to Current Lineage",
                                            &ShowCurrentLineage,
                                        )
                                    })
                                    .on_click(cx.listener(
                                        |this, _, _, cx| {
                                            let mode =
                                                if this.pending_history_mode.unwrap_or(this.mode)
                                                    == HistoryMode::Lineage
                                                {
                                                    HistoryMode::Log
                                                } else {
                                                    HistoryMode::Lineage
                                                };
                                            this.set_mode(mode, cx);
                                        },
                                    )),
                                )
                        },
                    )
                    .child(
                        IconButton::new("refresh", IconName::RotateCw)
                            .aria_label("Refresh")
                            .tooltip(Tooltip::for_action_title("Refresh", &Refresh))
                            .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
                    )
                    .child(
                        IconButton::new("collapse-all", IconName::ListCollapse)
                            .aria_label("Collapse All")
                            .tooltip(Tooltip::text("Collapse All"))
                            .on_click(cx.listener(|this, _, _, cx| this.collapse_tree(cx))),
                    )
                    .child({
                        let panel = cx.weak_entity();
                        PopoverMenu::new("more-actions")
                            .menu(move |window, cx| {
                                let order = panel
                                    .read_with(cx, |panel, _| panel.file_order.order())
                                    .ok()?;
                                let panel = panel.clone();
                                Some(ContextMenu::build(window, cx, |menu, _, _| {
                                    let (label, order) = if order == FileOrder::Repository {
                                        ("Sort by File Name", FileOrder::FileName)
                                    } else {
                                        ("Use Repository Order", FileOrder::Repository)
                                    };
                                    menu.entry(format!("KKW: {label}"), None, move |_, cx| {
                                        panel
                                            .update(cx, |panel, cx| {
                                                panel.set_file_order(order, cx);
                                            })
                                            .log_err();
                                    })
                                }))
                            })
                            .trigger(
                                IconButton::new("more", IconName::Ellipsis)
                                    .aria_label("More Actions")
                                    .tooltip(Tooltip::text("More Actions")),
                            )
                            .anchor(gpui::Anchor::TopRight)
                    }),
            )
            .child(
                uniform_list(
                    "kk-history",
                    row_count,
                    cx.processor(|this, range, _, cx| this.render_tree_rows(range, cx)),
                )
                .flex_1()
                .min_h_0()
                .track_scroll(&self.scroll_handle),
            )
            .children(self.context_menu.as_ref().map(|(menu, position, _)| {
                deferred(
                    anchored()
                        .position(*position)
                        .anchor(gpui::Anchor::TopLeft)
                        .child(menu.clone()),
                )
                .with_priority(1)
            }))
    }
}

fn canonical_local_path(file: &dyn language::File, path: &Path) -> Option<PathBuf> {
    if !file.is_local() {
        return None;
    }

    let parent = std::fs::canonicalize(path.parent()?).log_err()?;
    Some(parent.join(path.file_name()?))
}

#[cfg(test)]
mod tests {
    use super::remote_tests::run_kk;
    use super::*;
    use gpui::TestAppContext;
    use project::{FakeFs, Project};
    use std::path::Path;
    use workspace::{Item, MultiWorkspace};

    fn revision_ids(revisions: &[Revision]) -> Vec<String> {
        revisions
            .iter()
            .map(|revision| revision.full_commit_id.clone())
            .collect()
    }

    #[gpui::test(iterations = 3)]
    async fn kk_history_reveal_keeps_order_mode_and_branch_context(cx: &mut TestAppContext) {
        cx.background_executor.allow_parking();
        let (directory, target, middle, ancestor) = cx
            .background_spawn(async {
                let directory = tempfile::tempdir()?;
                let root = directory.path();
                run_kk(root, &["init", "."]).await?;
                run_kk(root, &["describe", "-m", "base"]).await?;
                run_kk(root, &["new", "-m", "branch middle"]).await?;
                run_kk(root, &["new", "-m", "branch target"]).await?;
                std::fs::write(root.join("file.txt"), "branch contents\n")?;
                let repository = kk_workbench::Repository::open(root, Path::new("kk")).await?;
                let snapshot = repository.snapshot(true).await?;
                let target = snapshot.exact_revision("@").await?;
                let middle = snapshot.exact_revision("@-").await?;
                let ancestor = snapshot.exact_revision("@--").await?;
                run_kk(
                    root,
                    &["new", &ancestor.full_commit_id, "-m", "current branch"],
                )
                .await?;
                for index in 0..51 {
                    run_kk(root, &["new", "-m", &format!("current {index}")]).await?;
                }

                run_kk(
                    root,
                    &[
                        "config",
                        "set",
                        "--repo",
                        "revsets.log",
                        "first_ancestors(@)",
                    ],
                )
                .await?;
                anyhow::Ok((directory, target, middle, ancestor))
            })
            .await
            .expect("repository");
        let root = std::fs::canonicalize(directory.path()).expect("canonical root");
        cx.update(|cx| {
            let mut settings = SettingsStore::test(cx);
            settings.set_user_settings(r#"{}"#, cx).expect("settings");
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            init(cx);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(&root, serde_json::json!({})).await;
        let project = Project::test(fs, [root.as_path()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = multi_workspace.read_with(cx, |view, _| view.workspace().clone());
        let panel = cx
            .update(|window, cx| KkPanel::load(workspace.downgrade(), window.to_async(cx)))
            .await
            .expect("panel");
        cx.condition(&panel, |panel, _| !panel.loading).await;
        let ancestor_id = TreeRowId::Revision(root.clone(), ancestor.full_change_id.clone());
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_panel(panel.clone(), window, cx);
            workspace.focus_panel::<KkPanel>(window, cx);
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        for mode in [HistoryMode::Lineage, HistoryMode::Log] {
            panel.update(cx, |panel, cx| panel.set_mode(mode, cx));
            cx.condition(&panel, |panel, _| !panel.loading).await;
            panel.read_with(cx, |panel, _| {
                assert!(panel.error.is_none(), "{:?}", panel.error);
                assert!(panel.repositories[0].history_targets.is_empty());
                assert!(
                    !revision_ids(&panel.repositories[0].history.revisions)
                        .contains(&target.full_commit_id)
                );
            });
            panel.update(cx, |panel, cx| {
                panel.reveal_revision(root.clone(), target.clone(), cx);
                panel.start_refresh(cx);
                panel.reveal_revision(root.clone(), ancestor.clone(), cx);
            });
            cx.condition(&panel, |panel, _| !panel.loading).await;
            let history = panel.update(cx, |panel, cx| {
                assert!(panel.error.is_none(), "{:?}", panel.error);
                assert_eq!(panel.mode, mode);
                assert_eq!(panel.history_preference.mode, mode);
                let repository = &panel.repositories[0];
                assert!(repository.history.revisions.len() > 50);
                assert!(revision_ids(&repository.history.revisions).contains(&target.full_commit_id));
                assert!(revision_ids(&repository.history.revisions).contains(&middle.full_commit_id));
                assert_ne!(revision_ids(&repository.history.revisions).first(), Some(&target.full_commit_id));
                assert!(matches!(repository.expanded.get(&target.full_commit_id), Some(RevisionFiles::Loaded(files)) if files.iter().any(|file| file.path == "file.txt")));
                let history = revision_ids(&repository.history.revisions);
                panel.update_tree(cx);
                assert_eq!(panel.tree_selection.selected, HashSet::from([ancestor_id.clone()]));
                assert!(!panel.reveal_selected);
                panel.refresh(cx);
                history
            });
            cx.condition(&panel, |panel, _| !panel.loading).await;
            cx.update(|window, cx| window.draw(cx).clear(cx));
            panel.read_with(cx, |panel, _| {
                assert!(panel.scroll_handle.0.borrow().base_handle.offset().y < gpui::px(0.));
                assert_eq!(
                    revision_ids(&panel.repositories[0].history.revisions),
                    history
                );
                assert_eq!(
                    panel.tree_selection.selected,
                    HashSet::from([ancestor_id.clone()])
                );
            });
            panel.update(cx, |panel, cx| {
                panel.reveal_revision(root.clone(), target.clone(), cx);
            });
            cx.condition(&panel, |panel, _| !panel.loading).await;
            panel.update(cx, |panel, cx| {
                panel.update_tree(cx);
                assert_eq!(
                    panel.tree_selection.selected,
                    HashSet::from([TreeRowId::Revision(
                        root.clone(),
                        target.full_change_id.clone()
                    )])
                );
                assert_eq!(
                    revision_ids(&panel.repositories[0].history.revisions),
                    history
                );
            });
        }

        let mut missing = target.clone();
        missing.full_change_id = "not-a-change-id".into();
        missing.full_commit_id = "0000000000000000000000000000000000000001".into();
        let (history, selection) = panel.update(cx, |panel, cx| {
            let history = revision_ids(&panel.repositories[0].history.revisions);
            let selection = panel.tree_selection.selected.clone();
            panel.reveal_revision(root.clone(), missing, cx);
            (history, selection)
        });
        cx.condition(&panel, |panel, _| !panel.loading).await;
        panel.update(cx, |panel, cx| {
            panel.update_tree(cx);
            assert!(panel.error.is_some());
            assert_eq!(
                revision_ids(&panel.repositories[0].history.revisions),
                history
            );
            assert_eq!(panel.tree_selection.selected, selection);
            panel.refresh(cx);
        });
        cx.condition(&panel, |panel, _| !panel.loading).await;
        panel.update(cx, |panel, cx| {
            assert!(panel.error.is_none(), "{:?}", panel.error);
            assert_eq!(
                revision_ids(&panel.repositories[0].history.revisions),
                history
            );
            panel.reveal_revision(root.clone(), ancestor, cx);
        });
        cx.condition(&panel, |panel, _| !panel.loading).await;
        panel.update(cx, |panel, cx| {
            panel.update_tree(cx);
            assert!(panel.error.is_none(), "{:?}", panel.error);
            assert_eq!(panel.tree_selection.selected, HashSet::from([ancestor_id]));
        });
    }

    #[gpui::test(iterations = 3)]
    async fn kk_history_mode_switch_updates_successful_repositories_and_preserves_pagination(
        cx: &mut TestAppContext,
    ) {
        cx.background_executor.allow_parking();
        let directories = cx
            .background_spawn(async {
                let mut directories = Vec::new();
                for _ in 0..2 {
                    let directory = tempfile::tempdir()?;
                    run_kk(directory.path(), &["init", "."]).await?;
                    for index in 0..51 {
                        run_kk(
                            directory.path(),
                            &["new", "-m", &format!("ancestor {index}")],
                        )
                        .await?;
                    }

                    run_kk(directory.path(), &["new", "-m", "sibling"]).await?;
                    run_kk(directory.path(), &["new", "@-", "-m", "current"]).await?;
                    std::fs::write(directory.path().join("file.txt"), "current contents\n")?;
                    run_kk(
                        directory.path(),
                        &["config", "set", "--repo", "revsets.log", "all()"],
                    )
                    .await?;
                    directories.push(directory);
                }

                anyhow::Ok(directories)
            })
            .await
            .expect("repositories");
        let roots = directories
            .iter()
            .map(|directory| std::fs::canonicalize(directory.path()).expect("canonical root"))
            .collect::<Vec<_>>();
        cx.update(|cx| {
            let mut settings = SettingsStore::test(cx);
            settings.set_user_settings(r#"{}"#, cx).expect("settings");
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            init(cx);
        });
        let fs = FakeFs::new(cx.executor());
        for root in &roots {
            fs.insert_tree(root, serde_json::json!({"file.txt": "current contents\n"}))
                .await;
        }

        let project = Project::test(fs, roots.iter().map(PathBuf::as_path), cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = multi_workspace.read_with(cx, |view, _| view.workspace().clone());
        let preference_key = workspace.update(cx, |workspace, _| {
            workspace.set_random_database_id();
            format!(
                "KKWorkbenchHistoryMode:{}",
                serde_json::to_string(&workspace.database_id().expect("workspace id"))
                    .expect("serialized id")
            )
        });
        let panel = cx
            .update(|window, cx| KkPanel::load(workspace.downgrade(), window.to_async(cx)))
            .await
            .expect("panel");
        cx.condition(&panel, |panel, _| !panel.loading).await;
        let history_ids = |panel: &KkPanel| {
            panel
                .repositories
                .iter()
                .map(|view| {
                    view.history
                        .revisions
                        .iter()
                        .map(|revision| revision.full_commit_id.clone())
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>()
        };
        panel.update(cx, |panel, cx| {
            for root in &roots {
                panel.history_limits.insert(root.clone(), 100);
            }

            panel.refresh(cx);
        });
        cx.condition(&panel, |panel, _| !panel.loading).await;
        let (original, selection) = panel.update(cx, |panel, cx| {
            assert_eq!(panel.repositories.len(), 2);
            assert!(panel.error.is_none(), "{:?}", panel.error);
            for root in &roots {
                panel.history_limits.insert(root.clone(), 100);
                panel
                    .history_loads
                    .insert(root.clone(), HistoryLoad::Failed);
            }

            for view in &mut panel.repositories {
                assert!(view.history.revisions.len() > 50);
                let revision = view.history.revisions.first().expect("current revision");
                view.expanded.insert(
                    revision.full_commit_id.clone(),
                    RevisionFiles::Loaded(Vec::new()),
                );
            }

            let view = panel.repositories.first().expect("first repository");
            let revision = view.history.revisions.first().expect("current revision");
            let selected = TreeRowId::Revision(
                view.snapshot.repository().root().to_owned(),
                revision.full_change_id.clone(),
            );
            panel.update_tree(cx);
            panel.tree_selection.selected.insert(selected);
            (history_ids(panel), panel.tree_selection.selected.clone())
        });
        let failing_root = roots[1].clone();
        cx.background_spawn(async move {
            run_kk(
                &failing_root,
                &[
                    "config",
                    "set",
                    "--repo",
                    "revsets.log",
                    "missing_function()",
                ],
            )
            .await
        })
        .await
        .expect("inject history failure");
        panel.update(cx, |panel, cx| {
            panel.set_mode(HistoryMode::Log, cx);
            assert_eq!(panel.mode, HistoryMode::Log);
            assert_eq!(panel.history_preference.mode, HistoryMode::Log);
            assert_eq!(history_ids(panel), original);
        });
        cx.condition(&panel, |panel, _| !panel.loading).await;
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.mode, HistoryMode::Log);
            assert_eq!(panel.history_preference.mode, HistoryMode::Log);
            let histories = history_ids(panel);
            for (index, view) in panel.repositories.iter().enumerate() {
                if view.snapshot.repository().root() == roots[1] {
                    assert_eq!(view.mode, HistoryMode::Lineage);
                    assert_eq!(histories[index], original[index]);
                } else {
                    assert_eq!(view.mode, HistoryMode::Log);
                    assert!(histories[index].len() > original[index].len());
                }
            }
            assert_eq!(panel.tree_selection.selected, selection);
            assert!(
                panel
                    .repositories
                    .iter()
                    .all(|view| !view.expanded.is_empty())
            );
            assert!(panel.pending_history_mode.is_none());
            assert!(
                panel
                    .error
                    .as_ref()
                    .is_some_and(|error| error.contains("missing_function"))
            );
            assert!(
                roots
                    .iter()
                    .all(|root| panel.history_limits.get(root) == Some(&100))
            );
            assert!(panel.history_loads.get(&roots[1]) == Some(&HistoryLoad::Failed));
            assert!(!panel.history_loads.contains_key(&roots[0]));
        });
        let failing_root = roots[1].clone();
        cx.background_spawn(async move {
            run_kk(
                &failing_root,
                &["config", "set", "--repo", "revsets.log", "all()"],
            )
            .await
        })
        .await
        .expect("restore history query");
        panel.update(cx, |panel, cx| panel.set_mode(HistoryMode::Log, cx));
        cx.condition(&panel, |panel, _| !panel.loading).await;
        panel.read_with(cx, |panel, _| {
            assert!(panel.error.is_none(), "{:?}", panel.error);
            assert_eq!(panel.mode, HistoryMode::Log);
            assert_eq!(panel.history_preference.mode, HistoryMode::Log);
            assert!(panel.history_loads.is_empty());
            assert_eq!(panel.tree_selection.selected, selection);
            assert!(
                panel
                    .repositories
                    .iter()
                    .all(|view| !view.expanded.is_empty())
            );
            assert!(
                roots
                    .iter()
                    .all(|root| panel.history_limits.get(root) == Some(&100))
            );
            for (view, original) in panel.repositories.iter().zip(&original) {
                assert!(view.history.revisions.len() > original.len());
            }
        });
        cx.condition(&panel, |_, cx| {
            db::kvp::KeyValueStore::global(cx)
                .read_kvp(&preference_key)
                .expect("saved preference")
                == Some("\"Log\"".into())
        })
        .await;
        panel.update(cx, |panel, cx| {
            panel.set_mode(HistoryMode::Lineage, cx);
            panel.start_refresh(cx);
            panel.set_mode(HistoryMode::Log, cx);
            panel.refresh(cx);
        });
        cx.condition(&panel, |panel, _| !panel.loading).await;
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.mode, HistoryMode::Log);
            assert_eq!(panel.history_preference.mode, HistoryMode::Log);
            assert!(panel.pending_history_mode.is_none());
            assert!(panel.error.is_none(), "{:?}", panel.error);
        });
        let stale_root = roots[1].clone();
        cx.background_spawn(async move {
            run_kk(
                &stale_root,
                &[
                    "config",
                    "set",
                    "--repo",
                    "snapshot.auto-update-stale",
                    "false",
                ],
            )
            .await?;
            run_kk(&stale_root, &["new", "root()", "--ignore-working-copy"]).await?;
            let repository = kk_workbench::Repository::open(&stale_root, Path::new("kk")).await?;
            let error = repository
                .snapshot(true)
                .await
                .err()
                .expect("stale snapshot must fail");
            anyhow::ensure!(kk_workbench::is_stale_workspace(&error), "{error:#}");
            anyhow::Ok(())
        })
        .await
        .expect("make repository stale");
        let previous = panel.read_with(cx, |panel, _| history_ids(panel));
        panel.update(cx, |panel, cx| panel.set_mode(HistoryMode::Lineage, cx));
        cx.condition(&panel, |panel, _| !panel.loading).await;
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.mode, HistoryMode::Lineage);
            assert_eq!(panel.history_preference.mode, HistoryMode::Lineage);
            for (index, view) in panel.repositories.iter().enumerate() {
                if view.snapshot.repository().root() == roots[1] {
                    assert_eq!(view.mode, HistoryMode::Log);
                    assert_eq!(history_ids(panel)[index], previous[index]);
                    assert!(view.stale);
                } else {
                    assert_eq!(view.mode, HistoryMode::Lineage);
                }
            }
        });
    }

    #[gpui::test(iterations = 3)]
    async fn removing_a_project_root_invalidates_only_its_captured_lifetime(
        cx: &mut TestAppContext,
    ) {
        cx.background_executor.allow_parking();
        cx.update(|cx| {
            let settings = SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/linked-first", serde_json::json!({})).await;
        fs.insert_tree("/second", serde_json::json!({})).await;
        let project =
            Project::test(fs, [Path::new("/linked-first"), Path::new("/second")], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |view, _| view.workspace().clone());
        let panel = cx
            .update(|window, cx| KkPanel::load(workspace.downgrade(), window.to_async(cx)))
            .await
            .expect("panel");
        let trees = project.read_with(cx, |project, cx| {
            project.visible_worktrees(cx).collect::<Vec<_>>()
        });
        let first_tree = trees
            .iter()
            .find(|tree| {
                tree.read_with(cx, |tree, _| {
                    tree.abs_path().as_ref() == Path::new("/linked-first")
                })
            })
            .expect("first tree")
            .clone();
        let first = kk_workbench::RepositoryLifetime::default();
        let second = kk_workbench::RepositoryLifetime::default();
        panel.update(cx, |panel, cx| {
            panel
                .repository_lifetimes
                .insert("/canonical-first".into(), first.clone());
            panel
                .repository_lifetimes
                .insert("/second".into(), second.clone());
            panel.repository_paths.insert(
                "/canonical-first".into(),
                vec![(first_tree.entity_id(), "/linked-first".into())],
            );
            assert!(panel.repository_is_open(Path::new("/canonical-first"), cx));
            assert!(panel.repository_is_open(Path::new("/second"), cx));
        });
        let first_id = first_tree.read_with(cx, |tree, _| tree.id());
        project.update(cx, |project, cx| project.remove_worktree(first_id, cx));
        cx.run_until_parked();
        assert!(first.check().is_err());
        assert!(second.check().is_ok());
        panel.read_with(cx, |panel, cx| {
            assert!(!panel.repository_is_open(Path::new("/canonical-first"), cx))
        });
        let replacement = project
            .update(cx, |project, cx| {
                project.create_worktree("/linked-first", true, cx)
            })
            .await
            .expect("reopen root");
        let fresh = kk_workbench::RepositoryLifetime::default();
        panel.update(cx, |panel, cx| {
            panel.repository_paths.insert(
                "/canonical-first".into(),
                vec![(replacement.entity_id(), "/linked-first".into())],
            );
            panel
                .repository_lifetimes
                .insert("/canonical-first".into(), fresh.clone());
            assert!(panel.repository_is_open(Path::new("/canonical-first"), cx));
        });
        assert!(first.check().is_err());
        assert!(fresh.check().is_ok());
        drop(panel);
        cx.run_until_parked();
        assert!(fresh.check().is_err());
        assert!(second.check().is_err());
    }

    #[gpui::test(iterations = 3)]
    async fn revision_retry_is_local_and_copies_full_unambiguous_identifiers(
        cx: &mut TestAppContext,
    ) {
        cx.background_executor.allow_parking();
        let (directory, snapshot, history) = cx
            .background_spawn(async {
                futures_lite::future::block_on(async {
                    let directory = tempfile::tempdir()?;
                    run_kk(directory.path(), &["init", "."]).await?;
                    std::fs::write(directory.path().join("file.txt"), "saved\n")?;
                    let repository =
                        kk_workbench::Repository::open(directory.path(), Path::new("kk")).await?;
                    let snapshot = repository.snapshot(true).await?;
                    let history = snapshot.history(HistoryMode::Lineage, 50).await?;
                    anyhow::Ok((directory, snapshot, history))
                })
            })
            .await
            .expect("prepare repository");
        let revision = history
            .revisions
            .iter()
            .find(|revision| revision.is_working_copy)
            .expect("working copy")
            .clone();
        let root = snapshot.repository().root().to_owned();
        cx.update(|cx| {
            let settings = SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            init(cx);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(&root, serde_json::json!({"file.txt": "saved\n"}))
            .await;
        let project = Project::test(fs, [root.as_path()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = multi_workspace.read_with(cx, |view, _| view.workspace().clone());
        let panel = cx
            .update(|window, cx| KkPanel::load(workspace.downgrade(), window.to_async(cx)))
            .await
            .expect("panel");
        panel.update(cx, |panel, cx| {
            assert!(panel.repositories.is_empty());
            panel.reveal_revision(root.clone(), revision.clone(), cx);
            assert!(panel.pending_reveal.is_some());
        });
        cx.condition(&panel, |panel, _| {
            !panel.loading && panel.pending_reveal.is_none()
        })
        .await;
        panel.read_with(cx, |panel, _| {
            assert!(panel.error.is_none(), "{:?}", panel.error);
            assert_eq!(
                panel.selected_change.as_ref().map(|(_, _, commit)| commit),
                Some(&revision.full_commit_id)
            );
        });
        let mut invalid = revision.clone();
        invalid.full_commit_id = "missing-revision".into();
        let unchanged = ChangedFile {
            path: "unrelated.txt".into(),
            change_type: kk_workbench::ChangeKind::Modified,
            renamed_from: None,
            has_conflict: false,
        };
        panel.update(cx, |panel, _| {
            panel.repositories = vec![RepositoryView {
                mode: HistoryMode::Lineage,
                stale: false,
                snapshot,
                history,
                expanded: HashMap::from([(
                    "unrelated".into(),
                    RevisionFiles::Loaded(vec![unchanged.clone()]),
                )]),
                comparison: None,
                history_targets: Vec::new(),
            }]
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_panel(panel.clone(), window, cx)
        });
        let snapshot = panel.read_with(cx, |panel, _| panel.repositories[0].snapshot.clone());
        let comparison = Comparison::new(&revision);
        panel.update(cx, |panel, _| {
            panel.repositories[0].comparison = Some(comparison.clone())
        });
        let page = cx
            .update(|window, cx| {
                KkMultiDiff::open(
                    snapshot.clone(),
                    comparison.diff_target(),
                    Vec::new(),
                    true,
                    workspace.downgrade(),
                    DiffOpenMode::Open,
                    window,
                    cx,
                )
            })
            .await
            .expect("comparison page");
        cx.condition(&page, |page, _| !page.loading).await;
        let new_base = snapshot
            .exact_revision("root()")
            .await
            .expect("new page base");
        let expected_label = Comparison::load(&snapshot, Comparison::new(&new_base).intent)
            .await
            .expect("expected comparison boundary")
            .diff_target()
            .tab_label();
        page.update_in(cx, |page, window, cx| {
            page.set_comparison_base(new_base, window, cx)
        });
        panel.update(cx, |panel, cx| panel.close_repository_comparison(0, cx));
        cx.condition(&page, |page, _| !page.loading).await;
        page.read_with(cx, |page, cx| {
            assert!(page.tab_content_text(0, cx).contains(&expected_label));
            assert!(page.tab_content_text(0, cx).contains("::@"));
        });
        assert!(
            panel.read_with(cx, |panel, _| panel.repositories[0].comparison.is_none()),
            "page base completion preserves the later sidebar close"
        );
        panel
            .update(cx, |panel, cx| {
                panel.load_files(0, invalid.clone(), false, cx);
                panel
                    .load_tasks
                    .remove(&format!(
                        "commit:{}:{}",
                        root.display(),
                        invalid.full_commit_id
                    ))
                    .expect("failed read task")
            })
            .await;
        panel.read_with(cx, |panel, _| {
            assert!(panel.error.is_none());
            assert!(matches!(panel.repositories[0].expanded.get(&invalid.full_commit_id), Some(RevisionFiles::Failed(error)) if !error.is_empty()));
        });
        panel
            .update(cx, |panel, cx| {
                panel.repositories[0].expanded.insert(
                    revision.full_commit_id.clone(),
                    RevisionFiles::Failed("temporary failure".into()),
                );
                panel.retry_commit(0, revision.clone(), cx);
                assert!(matches!(
                    panel.repositories[0].expanded.get(&revision.full_commit_id),
                    Some(RevisionFiles::Loading)
                ));
                panel
                    .load_tasks
                    .remove(&format!(
                        "commit:{}:{}",
                        root.display(),
                        revision.full_commit_id
                    ))
                    .expect("retry task")
            })
            .await;
        panel.update(cx, |panel, cx| {
            assert!(matches!(panel.repositories[0].expanded.get(&revision.full_commit_id), Some(RevisionFiles::Loaded(files)) if files.len() == 1 && files[0].path == "file.txt"));
            assert!(matches!(panel.repositories[0].expanded.get("unrelated"), Some(RevisionFiles::Loaded(files)) if files == &[unchanged]));
            panel.copy_revision_id(false, cx);
            assert_eq!(cx.read_from_clipboard().and_then(|item| item.text()), Some(revision.full_change_id.clone()));
            panel.copy_revision_id(true, cx);
            assert_eq!(cx.read_from_clipboard().and_then(|item| item.text()), Some(revision.full_commit_id.clone()));
            let mut rewritten = revision.clone();
            rewritten.full_commit_id = "rewritten".into();
            panel.repositories[0].history.revisions = vec![rewritten.clone()];
            assert_eq!(panel.selected_revision().expect("unique rewrite").1.full_commit_id, "rewritten");
            rewritten.full_commit_id = "divergent".into();
            panel.repositories[0].history.revisions.push(rewritten);
            assert!(panel.selected_revision().is_none());
        });
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.focus_panel::<KkPanel>(window, cx);
            });
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
            assert!(
                window
                    .context_stack()
                    .iter()
                    .any(|context| context.contains("KKWorkbenchLoaded"))
            );
        });
        panel.update(cx, |panel, cx| {
            panel.repositories[0].history.revisions.clear();
            cx.notify();
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
            assert!(
                !window
                    .context_stack()
                    .iter()
                    .any(|context| context.contains("KKWorkbenchLoaded"))
            );
        });
        drop(directory);
    }

    #[gpui::test(iterations = 3)]
    async fn file_classification_preserves_drafts_and_refreshes_categories(
        cx: &mut TestAppContext,
    ) {
        cx.background_executor.allow_parking();
        let (directory, snapshot) = cx
            .background_spawn(async {
                futures_lite::future::block_on(async {
                    let directory = tempfile::tempdir()?;
                    for arguments in [
                        vec!["init", "."],
                        vec!["config", "set", "--repo", "user.name", "KK Test"],
                        vec![
                            "config",
                            "set",
                            "--repo",
                            "user.email",
                            "test@example.invalid",
                        ],
                        vec!["new"],
                    ] {
                        run_kk(directory.path(), &arguments).await?;
                    }

                    std::fs::write(directory.path().join("file.txt"), "saved\n")?;
                    let repository =
                        kk_workbench::Repository::open(directory.path(), Path::new("kk")).await?;
                    anyhow::Ok((directory, repository.snapshot(true).await?))
                })
            })
            .await
            .expect("prepare isolated repository");
        let root = snapshot.repository().root().to_owned();
        cx.update(|cx| {
            let settings = SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(&root, serde_json::json!({"file.txt": "saved\n"}))
            .await;
        let project = Project::test(fs.clone(), [root.as_path()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |workspace, _| workspace.workspace().clone());
        let panel = cx
            .update(|window, cx| KkPanel::load(workspace.downgrade(), window.to_async(cx)))
            .await
            .expect("load KK panel");
        let comparison = cx
            .background_spawn({
                let snapshot = snapshot.clone();
                async move {
                    let parent = snapshot
                        .revisions(Some("@-"), 1)
                        .await?
                        .pop()
                        .ok_or_else(|| anyhow::anyhow!("missing parent"))?;
                    Comparison::load(&snapshot, Comparison::new(&parent).intent).await
                }
            })
            .await
            .expect("load comparison");
        let file = comparison.files.first().expect("changed file").clone();
        let buffer = project
            .update(cx, |project, cx| {
                project.open_local_buffer(root.join("file.txt"), cx)
            })
            .await
            .expect("open file");
        buffer.update(cx, |buffer, cx| {
            buffer.edit([(0..0, "draft\n")], None, cx);
        });
        panel
            .update(cx, |panel, cx| {
                panel.classify_file(
                    snapshot.clone(),
                    comparison.diff_target(),
                    comparison.intent.clone(),
                    file.clone(),
                    ClassificationTarget::Category(Category::Reviewed),
                    cx,
                )
            })
            .await;
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.pending_writes, 0);
            assert!(
                panel
                    .error
                    .as_deref()
                    .is_some_and(|error| error.contains("Save"))
            );
        });
        assert_eq!(
            buffer.read_with(cx, |buffer, _| buffer.text()),
            "draft\nsaved\n"
        );
        buffer.update(cx, |buffer, cx| {
            buffer.edit([(0..6, "")], None, cx);
        });
        assert!(!buffer.read_with(cx, |buffer, _| buffer.is_dirty()));
        panel.update(cx, |panel, cx| {
            panel.repositories = vec![RepositoryView {
                mode: HistoryMode::Lineage,
                stale: false,
                snapshot: panel
                    .bind_snapshot(snapshot.clone(), cx)
                    .expect("bind repository lifetime"),
                history: HistoryPage {
                    revisions: Vec::new(),
                    has_more: false,
                },
                expanded: HashMap::new(),
                comparison: Some(comparison.clone()),
                history_targets: Vec::new(),
            }];
        });
        let opened_diff = cx
            .update(|window, cx| {
                KkDiff::open(
                    snapshot.clone(),
                    comparison.diff_target(),
                    file.clone(),
                    true,
                    workspace.downgrade(),
                    DiffOpenMode::Open,
                    window,
                    cx,
                )
            })
            .await
            .expect("open comparison diff before file action");
        let native_diff = cx.update(|_, cx| {
            use workspace::item::ItemHandle;
            let editor = opened_diff
                .act_as_type(std::any::TypeId::of::<editor::Editor>(), cx)
                .expect("native editor")
                .downcast::<editor::Editor>()
                .expect("editor type");
            editor
                .read(cx)
                .buffer()
                .read(cx)
                .diff_for(buffer.read(cx).remote_id())
                .expect("native diff")
        });
        let (first, duplicate) = panel.update(cx, |panel, cx| {
            let first = panel.classify_file(
                snapshot.clone(),
                comparison.diff_target(),
                comparison.intent.clone(),
                file.clone(),
                ClassificationTarget::Category(Category::Reviewed),
                cx,
            );
            let duplicate = panel.classify_file(
                snapshot.clone(),
                comparison.diff_target(),
                comparison.intent.clone(),
                file,
                ClassificationTarget::Category(Category::Reviewed),
                cx,
            );
            let projected = panel.list_previews.files(&panel.repositories[0]);
            assert!(
                projected
                    .iter()
                    .all(|file| file.category == Some(Category::Reviewed))
            );
            assert_eq!(
                projected.len(),
                1,
                "duplicate requests must not duplicate a row"
            );
            assert!(
                projected[0].pending,
                "list updates before any backend work runs"
            );
            (first, duplicate)
        });
        buffer.update(cx, |buffer, cx| {
            buffer.edit([(buffer.len()..buffer.len(), "later saved\n")], None, cx);
            std::fs::write(root.join("file.txt"), buffer.text()).expect("save native file");
            buffer.did_save(buffer.version(), None, cx);
            buffer.edit([(0..0, "queued draft\n")], None, cx);
        });
        fs.insert_file(root.join("file.txt"), b"saved\nlater saved\n".to_vec())
            .await;
        first.await;
        duplicate.await;
        cx.run_until_parked();
        assert_eq!(
            native_diff.read_with(cx, |diff, cx| diff.base_text_buffer().read(cx).text()),
            "saved\n"
        );
        panel
            .update(cx, |panel, _| {
                std::mem::replace(&mut panel.refresh_task, Task::ready(()))
            })
            .await;
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.pending_writes, 0);
            assert!(
                panel.operation_error.is_none(),
                "{:?}",
                panel.operation_error
            );
            assert!(panel.error.is_none(), "{:?}", panel.error);
            let comparison = panel
                .repositories
                .first()
                .and_then(|repository| repository.comparison.as_ref())
                .expect("comparison remains open");
            assert_eq!(comparison.files.len(), 1);
            assert_eq!(comparison.categories.len(), 1);
            assert_eq!(comparison.categories[0].category, Category::Reviewed);
            assert_eq!(comparison.categories[0].files[0].path, "file.txt");
            assert!(
                panel
                    .list_previews
                    .files(&panel.repositories[0])
                    .iter()
                    .all(|file| !file.pending)
            );
        });
        assert_eq!(
            buffer.read_with(cx, |buffer, _| buffer.text()),
            "queued draft\nsaved\nlater saved\n"
        );
        assert_eq!(
            std::fs::read(root.join("file.txt")).expect("read native file"),
            b"saved\nlater saved\n"
        );
        buffer.update(cx, |buffer, cx| {
            buffer.edit([(0..13, "")], None, cx);
        });
        assert!(!buffer.read_with(cx, |buffer, _| buffer.is_dirty()));
        let (snapshot, category) = panel.read_with(cx, |panel, _| {
            let repository = &panel.repositories[0];
            (
                repository.snapshot.clone(),
                repository
                    .comparison
                    .as_ref()
                    .expect("comparison")
                    .categories[0]
                    .clone(),
            )
        });
        panel
            .update_in(cx, |panel, window, cx| {
                panel.mutate_file(
                    snapshot,
                    super::super::kk_diff::DiffTarget::change(&category.revision),
                    category.files[0].clone(),
                    FileMutationAction::Move(FileMoveDirection::Child),
                    window,
                    cx,
                )
            })
            .await;
        cx.run_until_parked();
        panel
            .update(cx, |panel, _| {
                std::mem::replace(&mut panel.refresh_task, Task::ready(()))
            })
            .await;
        let (snapshot, comparison) = panel.read_with(cx, |panel, _| {
            assert!(
                panel.operation_error.is_none(),
                "{:?}",
                panel.operation_error
            );
            let repository = &panel.repositories[0];
            let comparison = repository.comparison.as_ref().expect("comparison").clone();
            assert!(comparison.categories.is_empty());
            assert_eq!(comparison.files.len(), 1);
            (repository.snapshot.clone(), comparison)
        });
        let file = comparison.files[0].clone();
        let cancelled = panel.update_in(cx, |panel, window, cx| {
            panel.mutate_file(
                snapshot.clone(),
                comparison.diff_target(),
                file.clone(),
                FileMutationAction::Revert,
                window,
                cx,
            )
        });
        assert!(cx.has_pending_prompt());
        cx.simulate_prompt_answer("Cancel");
        cancelled.await;
        assert!(root.join("file.txt").exists());
        assert_eq!(panel.read_with(cx, |panel, _| panel.pending_writes), 0);

        let dirty = panel.update_in(cx, |panel, window, cx| {
            panel.mutate_file(
                snapshot.clone(),
                comparison.diff_target(),
                file.clone(),
                FileMutationAction::Revert,
                window,
                cx,
            )
        });
        buffer.update(cx, |buffer, cx| {
            buffer.edit([(0..0, "late draft\n")], None, cx);
        });
        cx.simulate_prompt_answer("Revert File");
        dirty.await;
        assert_eq!(
            buffer.read_with(cx, |buffer, _| buffer.text()),
            "late draft\nsaved\nlater saved\n"
        );
        assert_eq!(
            std::fs::read(root.join("file.txt")).expect("original file retained"),
            b"saved\nlater saved\n"
        );
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.pending_writes, 0);
            assert!(
                panel
                    .operation_error
                    .as_ref()
                    .is_some_and(|(_, error)| error.contains("draft"))
            );
        });
        buffer.update(cx, |buffer, cx| {
            buffer.edit([(0..11, "")], None, cx);
        });
        assert!(!buffer.read_with(cx, |buffer, _| buffer.is_dirty()));
        let revert = panel.update_in(cx, |panel, window, cx| {
            panel.mutate_file(
                snapshot,
                comparison.diff_target(),
                file,
                FileMutationAction::Revert,
                window,
                cx,
            )
        });
        cx.simulate_prompt_answer("Revert File");
        revert.await;
        cx.run_until_parked();
        panel
            .update(cx, |panel, _| {
                std::mem::replace(&mut panel.refresh_task, Task::ready(()))
            })
            .await;
        panel.read_with(cx, |panel, _| {
            assert!(
                panel.operation_error.is_none(),
                "{:?}",
                panel.operation_error
            );
            assert_eq!(panel.pending_writes, 0);
        });
        assert!(!root.join("file.txt").exists());
        drop(directory);
    }
}
