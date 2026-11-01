use anyhow::{Context as _, Result, ensure};
use buffer_diff::{BufferDiff, RestoreDiffOperations};
use db::kvp::KeyValueStore;
use editor::items::{SerializedBufferContents, SerializedEditorState};
use editor::{
    Editor, EditorEvent, EditorSettings, SelectionEffects, SplittableEditor, scroll::Autoscroll,
};
use file_icons::FileIcons;
use futures::{FutureExt as _, future::Shared};
use gpui::{
    App, AppContext as _, ClipboardItem, Context, Entity, EventEmitter, FocusHandle, Focusable,
    Render, SharedString, Subscription, Task, WeakEntity, Window, actions,
};
use kk_workbench::{
    Category, ChangeKind, ChangedFile, ChangedLine, ClassificationPreview, ClassificationTarget,
    DiffScope, FileSelection, LineKind, LineRange, LineSelection, Revision, SelectionSide,
    Snapshot, TransferDirection,
};
use language::{Buffer, BufferEvent, Capability, Point, ToPoint as _};
use multi_buffer::MultiBuffer;
use project::Project;
use serde::{Deserialize, Serialize};
use settings::Settings;
use std::{any::TypeId, collections::HashSet, path::PathBuf, sync::Arc};
use ui::prelude::*;
use util::ResultExt as _;
use workspace::{
    Item, ItemId, PreviewTabsSettings, SerializableItem, Workspace, WorkspaceId,
    item::{ItemBufferKind, ItemEvent, ItemHandle as _, SaveOptions},
    searchable::{SearchableItem, SearchableItemHandle},
};

pub(super) fn init(cx: &mut App) {
    workspace::register_serializable_item::<KkDiff>(cx);
    workspace::register_serializable_item::<super::kk_multi_diff::KkMultiDiff>(cx);
}

fn session_namespace(workspace_id: WorkspaceId) -> String {
    format!("KKDiffTabs:{}", i64::from(workspace_id))
}

#[derive(Deserialize, Serialize)]
struct SavedDiffTab {
    root: PathBuf,
    target: DiffTarget,
    file: ChangedFile,
    working_copy: bool,
    operation: String,
    host: Option<String>,
    buffer: Option<SerializedBufferContents>,
    split: bool,
    left: bool,
    editor: SerializedEditorState,
}

#[derive(Default)]
struct DiffLoadState {
    saved_tab: Option<SavedDiffTab>,
    saved_buffer: Option<SerializedBufferContents>,
    existing_view: Option<Entity<KkDiff>>,
}

#[derive(Clone, PartialEq, Eq)]
struct DiffIdentity {
    root: PathBuf,
    scope: String,
    path: String,
    working_copy: bool,
    presentation: DiffPresentation,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DiffPresentation {
    Text,
    FileInformation,
    LfsPointers,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum DiffOpenMode {
    Preview,
    Open,
    Hidden,
}

impl DiffIdentity {
    fn title(&self, target: &DiffTarget, file: &ChangedFile) -> SharedString {
        let suffix = match self.presentation {
            DiffPresentation::Text => "",
            DiffPresentation::FileInformation => " (File information)",
            DiffPresentation::LfsPointers => " (LFS pointers)",
        };
        let revision = target.tab_label();
        let revision = match (&target.scope, file.change_type) {
            (DiffScope::Change(_), ChangeKind::Added) => format!("added in {revision}"),
            (DiffScope::Change(_), ChangeKind::Deleted) => format!("removed in {revision}"),
            _ => revision,
        };
        let name = self.path.rsplit('/').next().unwrap_or(&self.path);
        format!("{name} ({revision}){suffix}").into()
    }
}

actions!(
    kk_workbench,
    [
        CopyDiff,
        OpenFileInProject,
        MarkPending,
        MarkReviewed,
        MarkRejected,
        MarkDebug,
        ReturnToComparison
    ]
);

#[derive(Clone, Deserialize, Serialize)]
pub(super) struct DiffTarget {
    pub scope: DiffScope,
    pub identity: String,
    pub label: String,
    reference: String,
    pub comparison: Option<super::kk_comparison::ComparisonIntent>,
    #[serde(default)]
    pub revision: Option<Revision>,
    category: Option<Category>,
}

impl DiffTarget {
    pub(super) fn tab_label(&self) -> String {
        let label = if let Some(comparison) = &self.comparison
            && matches!(self.scope, DiffScope::Comparison { .. })
        {
            comparison.range_start_change_id.as_ref().map_or_else(
                || format!("@ vs {}", comparison.target_revset),
                |start| format!("{}::@", start.chars().take(8).collect::<String>()),
            )
        } else {
            self.label.clone()
        };
        label
            .split_inclusive(|character: char| !character.is_alphanumeric() && character != '_')
            .fold(String::new(), |mut result, part| {
                let word = part.trim_end_matches(|character: char| {
                    !character.is_alphanumeric() && character != '_'
                });
                if word.len() > 8 && word.bytes().all(|byte| (b'k'..=b'z').contains(&byte)) {
                    result.extend(word.chars().take(8));
                    result.push_str(&part[word.len()..]);
                } else {
                    result.push_str(part);
                }
                result
            })
    }

    pub async fn resolve(&self, snapshot: &Snapshot) -> Result<(Self, bool)> {
        if let DiffScope::Change(commit_id) = &self.scope {
            let mut revisions = snapshot.revisions(Some(&self.identity), 3).await?;
            let revision = if let Some(index) = revisions
                .iter()
                .position(|revision| revision.full_commit_id == *commit_id)
            {
                revisions.remove(index)
            } else {
                ensure!(revisions.len() == 1, "Change no longer resolves uniquely");
                revisions.pop().context("Change no longer exists")?
            };
            let mut target = Self::change(&revision);
            target.comparison = self.comparison.clone();
            Ok((target, revision.is_working_copy))
        } else {
            Ok((self.clone(), true))
        }
    }

    pub fn change(revision: &Revision) -> Self {
        Self {
            scope: DiffScope::Change(revision.full_commit_id.clone()),
            identity: revision.revset(),
            label: revision.change_id.clone(),
            reference: revision.full_change_id.chars().take(8).collect(),
            comparison: None,
            category: Category::from_revision(revision),
            revision: Some(revision.clone()),
        }
    }

    pub fn comparison(
        identity: String,
        base: String,
        label: String,
        range_start: Option<&str>,
    ) -> Self {
        Self {
            comparison: Some(super::kk_comparison::ComparisonIntent::for_target(
                identity.clone(),
                label.clone(),
                range_start,
            )),
            category: None,
            revision: None,
            reference: range_start
                .map(|identifier| format!("{}::@", identifier.chars().take(8).collect::<String>()))
                .unwrap_or_else(|| format!("({base})..@")),
            scope: DiffScope::Comparison {
                base,
                target: "@".into(),
            },
            identity: format!("comparison:{identity}"),
            label: format!("{label} → @"),
        }
    }
}

struct DiffContents {
    base: Option<Arc<str>>,
    base_revision: Option<String>,
    historical: Option<String>,
    divergent: bool,
    presentation: DiffPresentation,
    revision: Option<Revision>,
}

struct DiffAddon(WeakEntity<KkDiff>);

pub(super) fn file_status(file: &ChangedFile) -> git::status::FileStatus {
    use git::status::{FileStatus, StatusCode, TrackedStatus, UnmergedStatus, UnmergedStatusCode};
    if file.has_conflict {
        return FileStatus::Unmerged(UnmergedStatus {
            first_head: UnmergedStatusCode::Updated,
            second_head: UnmergedStatusCode::Updated,
        });
    }
    FileStatus::Tracked(TrackedStatus {
        index_status: StatusCode::Unmodified,
        worktree_status: match file.change_type {
            ChangeKind::Modified => StatusCode::Modified,
            ChangeKind::Added => StatusCode::Added,
            ChangeKind::Deleted => StatusCode::Deleted,
            ChangeKind::Renamed => StatusCode::Renamed,
            ChangeKind::Copied => StatusCode::Copied,
        },
    })
}

impl editor::Addon for DiffAddon {
    fn override_status_for_buffer_id(
        &self,
        _: language::BufferId,
        cx: &App,
    ) -> Option<git::status::FileStatus> {
        Some(file_status(&self.0.upgrade()?.read(cx).file))
    }
    fn extend_context_menu(
        &self,
        menu: ui::ContextMenu,
        _: &mut Window,
        cx: &mut App,
    ) -> ui::ContextMenu {
        let Some(view) = self.0.upgrade() else {
            return menu;
        };
        let view = view.read(cx);
        let menu = menu
            .separator()
            .action("KKW: Open File in Project", Box::new(OpenFileInProject))
            .action("KKW: Copy Diff", Box::new(CopyDiff))
            .when(
                view.identity.presentation == DiffPresentation::Text,
                |menu| {
                    menu.action(
                        "KKW: Search Selection in Changes",
                        Box::new(super::kk_search::SearchSelectionInChanges),
                    )
                },
            );
        if view.target.comparison.is_none() || view.identity.presentation != DiffPresentation::Text
        {
            return menu;
        }

        menu.separator()
            .when(view.target.category != Some(Category::Reviewed), |menu| {
                menu.action("KKW: Reviewed", Box::new(MarkReviewed))
            })
            .when(view.target.category != Some(Category::Pending), |menu| {
                menu.action("KKW: Pending", Box::new(MarkPending))
            })
            .when(view.target.category != Some(Category::Debug), |menu| {
                menu.action("KKW: Debug", Box::new(MarkDebug))
            })
            .when(view.target.category != Some(Category::Rejected), |menu| {
                menu.action("KKW: Rejected", Box::new(MarkRejected))
            })
            .when(view.target.category.is_some(), |menu| {
                menu.action("KKW: Comparison", Box::new(ReturnToComparison))
            })
    }

    fn to_any(&self) -> &dyn std::any::Any {
        self
    }
}

impl DiffContents {
    async fn text(snapshot: &Snapshot, mut bytes: Vec<u8>, resolve_lfs: bool) -> Result<String> {
        if resolve_lfs && let Some(pointer) = kk_workbench::LfsPointer::parse(&bytes) {
            bytes = snapshot.repository().lfs_content(&pointer).await?;
        }

        ensure!(
            !bytes.contains(&0),
            "Binary file content requires the file information view"
        );
        let mut text = String::from_utf8(bytes).context("File is not UTF-8")?;
        if text.starts_with('\u{feff}') {
            text.remove(0);
        }

        Ok(text)
    }

    async fn load(
        snapshot: &Snapshot,
        target: &DiffTarget,
        file: &ChangedFile,
        live: bool,
        draft: Option<Vec<u8>>,
        presentation: Option<DiffPresentation>,
    ) -> Result<Self> {
        let (base_revision, target_revision, divergent, revision) = match &target.scope {
            DiffScope::Comparison { base, target } => {
                (Some(base.clone()), target.clone(), false, None)
            }
            DiffScope::Change(revision) => {
                let revisions = snapshot.revisions(Some(revision), 2).await?;
                let selected = revisions.first().context("Change no longer exists")?;
                (
                    (selected.parent_commit_ids.len() <= 1).then(|| {
                        selected
                            .parent_commit_ids
                            .first()
                            .cloned()
                            .unwrap_or_else(|| "root()".into())
                    }),
                    revision.clone(),
                    snapshot.revisions(Some(&selected.revset()), 2).await?.len() > 1,
                    Some(selected.clone()),
                )
            }
        };
        let original_path = file.renamed_from.as_deref().unwrap_or(&file.path);
        let merged_parent = if base_revision.is_none() {
            Some(snapshot.merged_parent_file(&target_revision, file).await?)
        } else {
            None
        };
        let text = async {
            let bytes = if let Some((information, bytes)) = &merged_parent {
                ensure!(
                    information.unavailable.is_none(),
                    "{}",
                    information.unavailable.as_deref().unwrap_or_default()
                );
                bytes.clone()
            } else if let Some(base_revision) = &base_revision {
                snapshot
                    .file_content_if_exists(base_revision, original_path)
                    .await?
            } else {
                None
            };
            let base = if let Some(bytes) = bytes {
                Some(Arc::<str>::from(Self::text(snapshot, bytes, true).await?))
            } else {
                None
            };
            let text = if live {
                match &draft {
                    // Native drafts are already decoded text, including literal control characters.
                    Some(bytes) => {
                        String::from_utf8(bytes.clone()).context("Draft is not UTF-8")?
                    }
                    None => {
                        let bytes =
                            match snapshot.repository().working_file_content(&file.path).await {
                                Ok(bytes) => bytes,
                                Err(error) => {
                                    if file.change_type == ChangeKind::Deleted
                                        && snapshot
                                            .repository()
                                            .working_file_information(&file.path, None)
                                            .await?
                                            .exists
                                            == Some(false)
                                    {
                                        Vec::new()
                                    } else {
                                        return Err(error);
                                    }
                                }
                            };
                        Self::text(snapshot, bytes, false).await?
                    }
                }
            } else {
                let bytes = snapshot
                    .file_content_if_exists(&target_revision, &file.path)
                    .await?
                    .unwrap_or_default();
                Self::text(snapshot, bytes, true).await?
            };
            anyhow::Ok(Self {
                base,
                base_revision: base_revision.clone(),
                historical: (!live).then_some(text),
                divergent,
                presentation: DiffPresentation::Text,
                revision: revision.clone(),
            })
        };
        if presentation.is_none_or(|presentation| presentation == DiffPresentation::Text) {
            match text.await {
                Ok(contents) => return Ok(contents),
                Err(error) if presentation == Some(DiffPresentation::Text) => return Err(error),
                Err(error) => log::debug!("KK diff fallback: {error:#}"),
            }
        }

        let original = if let Some((information, _)) = &merged_parent {
            information.clone()
        } else {
            snapshot
                .file_information(
                    base_revision.as_deref().context("Missing diff base")?,
                    original_path,
                )
                .await?
        };
        let modified = if live {
            snapshot
                .repository()
                .working_file_information(&file.path, draft.as_deref())
                .await?
        } else {
            snapshot
                .file_information(&target_revision, &file.path)
                .await?
        };
        if presentation != Some(DiffPresentation::FileInformation)
            && (presentation == Some(DiffPresentation::LfsPointers)
                || original.pointer.is_some()
                || modified.pointer.is_some())
            && let (Some(original), Some(modified)) =
                (original.pointer_text(), modified.pointer_text())
        {
            return Ok(Self {
                base: Some(original.into()),
                base_revision: base_revision.clone(),
                historical: Some(modified),
                divergent,
                presentation: DiffPresentation::LfsPointers,
                revision: revision.clone(),
            });
        }

        Ok(Self {
            base: Some(original.format().into()),
            base_revision: base_revision.clone(),
            historical: Some(modified.format()),
            divergent,
            presentation: DiffPresentation::FileInformation,
            revision: revision.clone(),
        })
    }
}

#[derive(Clone)]
struct PendingClassification {
    model: Entity<ClassificationPreview>,
    waiting: HashSet<u64>,
    latest: Option<(u64, Snapshot)>,
    uncertain: bool,
    uncertain_ids: HashSet<u64>,
}

pub(super) struct KkDiff {
    identity: DiffIdentity,
    target: DiffTarget,
    pub(super) file: ChangedFile,
    snapshot: Option<Snapshot>,
    workspace: WeakEntity<Workspace>,
    pub(super) buffer: Entity<Buffer>,
    pub(super) diff: Entity<BufferDiff>,
    editor: Entity<SplittableEditor>,
    controls: Entity<super::kk_diff_controls::KkDiffControls>,
    base: Option<Arc<str>>,
    base_revision: Option<String>,
    writes: Shared<Task<Result<(), String>>>,
    update_task: Task<()>,
    refresh_task: Task<()>,
    refresh_generation: u64,
    refreshing: Option<(u64, Snapshot)>,
    classification_sequence: u64,
    classification: Option<PendingClassification>,
    preview_task: Task<()>,
    error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl KkDiff {
    pub(super) fn conflict_comparison(
        title: String,
        path: String,
        buffer: Entity<Buffer>,
        diff: Entity<BufferDiff>,
        base: Arc<str>,
        project: Entity<Project>,
        workspace: Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let identity = DiffIdentity {
            root: PathBuf::new(),
            scope: title.clone(),
            path: path.clone(),
            working_copy: false,
            presentation: DiffPresentation::Text,
        };
        let target = DiffTarget {
            scope: DiffScope::Comparison {
                base: String::new(),
                target: String::new(),
            },
            identity: title.clone(),
            label: title,
            reference: String::new(),
            comparison: None,
            revision: None,
            category: None,
        };
        let file = ChangedFile {
            path,
            change_type: ChangeKind::Modified,
            renamed_from: None,
            has_conflict: false,
        };
        let view = Self::new(
            identity,
            target,
            file,
            buffer,
            diff,
            Some(base),
            project,
            workspace,
            window,
            cx,
        );
        view.editor
            .read(cx)
            .rhs_editor()
            .clone()
            .update(cx, |editor, cx| {
                let first = editor
                    .buffer()
                    .read(cx)
                    .snapshot(cx)
                    .diff_hunks()
                    .next()
                    .map(|hunk| Point::new(hunk.row_range.start.0, 0));
                if let Some(position) = first {
                    editor.change_selections(
                        SelectionEffects::scroll(Autoscroll::center()),
                        window,
                        cx,
                        |selections| selections.select_ranges([position..position]),
                    );
                }
            });
        view
    }

    pub(super) fn working_path(&self, cx: &App) -> Option<PathBuf> {
        self.has_working_copy_buffer(cx)
            .then(|| {
                self.buffer
                    .read(cx)
                    .file()
                    .and_then(|file| file.file_system_abs_path(cx))
            })
            .flatten()
    }
    pub(super) fn has_pending_classification(&self) -> bool {
        self.classification
            .as_ref()
            .is_some_and(|preview| preview.uncertain || !preview.waiting.is_empty())
    }
    pub(super) fn is_text(&self) -> bool {
        self.identity.presentation == DiffPresentation::Text
    }
    pub(super) fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
    pub(super) fn snapshot(&self) -> Option<&Snapshot> {
        self.snapshot.as_ref()
    }
    pub(super) fn target(&self) -> &DiffTarget {
        &self.target
    }

    fn has_working_copy_buffer(&self, cx: &App) -> bool {
        project::File::from_dyn(self.buffer.read(cx).file()).is_some()
    }

    pub(super) fn blame_targets(
        &self,
        cx: &App,
    ) -> Vec<(Entity<Editor>, Entity<Buffer>, super::kk_blame::BlameTarget)> {
        use super::kk_blame::BlameTarget;
        let Some(snapshot) = self
            .snapshot
            .clone()
            .filter(|_| self.identity.presentation == DiffPresentation::Text)
        else {
            return Vec::new();
        };
        let split = self.editor.read(cx);
        let modified = match &self.target.scope {
            DiffScope::Change(_) => self.target.identity.clone(),
            DiffScope::Comparison { target, .. } => target.clone(),
        };
        let target = if self.has_working_copy_buffer(cx) {
            BlameTarget::WorkingFile {
                absolute: self.identity.root.join(&self.identity.path),
                remote: self.workspace.upgrade().and_then(|workspace| {
                    workspace
                        .read(cx)
                        .project()
                        .read(cx)
                        .kk_workbench_client(cx)
                }),
            }
        } else {
            BlameTarget::Revision {
                snapshot: snapshot.clone(),
                revision: modified,
                path: self.identity.path.clone(),
                refresh_buffer: false,
                empty: false,
            }
        };
        let mut targets = vec![(split.rhs_editor().clone(), self.buffer.clone(), target)];
        if let (Some(left), Some(original)) = (split.lhs_editor(), &self.base_revision) {
            targets.push((
                left.clone(),
                self.diff.read(cx).base_text_buffer().clone(),
                BlameTarget::Revision {
                    snapshot,
                    revision: original.clone(),
                    path: self
                        .file
                        .renamed_from
                        .as_deref()
                        .unwrap_or(&self.file.path)
                        .to_owned(),
                    refresh_buffer: false,
                    empty: self.base.is_none(),
                },
            ));
        }

        targets
    }

    pub(super) fn open(
        snapshot: Snapshot,
        target: DiffTarget,
        file: ChangedFile,
        working_copy: bool,
        workspace: WeakEntity<Workspace>,
        mode: DiffOpenMode,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Entity<Self>>> {
        if let Err(error) = snapshot.repository().check_lifetime() {
            return Task::ready(Err(error));
        }
        if let Some(existing) = Self::reopen(
            snapshot.clone(),
            &target,
            &file,
            working_copy,
            workspace.clone(),
            mode,
            window,
            cx,
        ) {
            return Task::ready(Ok(existing));
        }

        Self::load(
            snapshot,
            target,
            file,
            working_copy,
            workspace,
            DiffLoadState::default(),
            mode,
            window,
            cx,
        )
    }

    pub(super) fn reopen(
        snapshot: Snapshot,
        target: &DiffTarget,
        file: &ChangedFile,
        working_copy: bool,
        workspace: WeakEntity<Workspace>,
        mode: DiffOpenMode,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Entity<Self>> {
        snapshot.repository().check_lifetime().ok()?;
        if let Some(workspace) = workspace.upgrade() {
            let existing = workspace.read(cx).items_of_type::<Self>(cx).find(|item| {
                let view = item.read(cx);
                view.identity.root == snapshot.repository().root()
                    && (view.identity.scope == target.identity
                        || matches!(&target.scope, DiffScope::Change(commit) if view.identity.scope == format!("{}:{commit}", target.identity)))
                    && view.target.reference == target.reference
                    && view.target.category == target.category
                    && view.identity.path == file.path
                    && view.identity.working_copy == working_copy
                    && view
                        .working_path(cx)
                        .is_none_or(|path| path == snapshot.repository().root().join(&file.path))
            });
            if let Some(existing) = existing {
                workspace.update(cx, |workspace, cx| {
                    Self::show(&existing, mode, workspace, window, cx);
                });
                existing.update(cx, |view, cx| view.refresh(snapshot, cx));
                return Some(existing);
            }
        }
        None
    }

    pub(super) fn load_for_multibuffer(
        snapshot: Snapshot,
        target: DiffTarget,
        file: ChangedFile,
        working_copy: bool,
        workspace: WeakEntity<Workspace>,
        saved_buffer: Option<SerializedBufferContents>,
        existing_view: Option<Entity<Self>>,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Entity<Self>>> {
        Self::load(
            snapshot,
            target,
            file,
            working_copy,
            workspace,
            DiffLoadState {
                saved_tab: None,
                saved_buffer,
                existing_view,
            },
            DiffOpenMode::Hidden,
            window,
            cx,
        )
    }

    fn load(
        snapshot: Snapshot,
        target: DiffTarget,
        file: ChangedFile,
        working_copy: bool,
        workspace: WeakEntity<Workspace>,
        state: DiffLoadState,
        mode: DiffOpenMode,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Entity<Self>>> {
        let Some(workspace) = workspace.upgrade() else {
            return Task::ready(Err(anyhow::anyhow!("Workspace was closed")));
        };
        let mut identity = DiffIdentity {
            root: snapshot.repository().root().to_owned(),
            scope: target.identity.clone(),
            path: file.path.clone(),
            working_copy,
            presentation: DiffPresentation::Text,
        };
        let project = workspace.read(cx).project().clone();
        let mut restored = state.saved_tab;
        let mut saved_buffer = state
            .saved_buffer
            .or_else(|| restored.as_mut().and_then(|saved| saved.buffer.take()));
        let existing_view = state.existing_view;
        let file_for_view = file.clone();
        let draft = project
            .read(cx)
            .opened_buffers(cx)
            .into_iter()
            .find_map(|buffer| {
                let buffer = buffer.read(cx);
                (buffer.is_dirty()
                    && buffer
                        .file()
                        .and_then(|file| file.file_system_abs_path(cx))
                        .as_ref()
                        == Some(&identity.root.join(&identity.path)))
                .then(|| buffer.text().into_bytes())
            })
            .or_else(|| {
                saved_buffer
                    .as_ref()
                    .map(|buffer| buffer.text().as_bytes().to_vec())
            });
        window.spawn(cx, async move |cx| {
            let performance = snapshot.repository().diagnostics().start(
                "diff.open",
                None,
                serde_json::json!({"file": file.path, "root": snapshot.repository().root()}),
            );
            let snapshot = snapshot.with_performance_context(performance.context().cloned());
            let result = async {
                let _opening = snapshot.repository().begin_diff_opening().await?;
                let snapshot_for_view = snapshot.clone();
                let path = identity.root.join(&file.path);
                let target_for_read = target.clone();
                let DiffContents {
                    base,
                    base_revision,
                    historical,
                    divergent,
                    presentation,
                    revision,
                } = cx
                    .background_spawn(async move {
                        let preparing = snapshot.repository().diagnostics().start(
                            "diff.prepare",
                            snapshot.repository().performance_context(),
                            serde_json::Value::Null,
                        );
                        let result = DiffContents::load(
                            &snapshot,
                            &target_for_read,
                            &file,
                            working_copy,
                            draft,
                            None,
                        )
                        .await;
                        preparing.finish(
                            if result.is_ok() { "completed" } else { "error" },
                            serde_json::Value::Null,
                        );
                        result
                    })
                    .await?;
                let mut target = target;
                target.revision = revision;
                identity.presentation = presentation;
                if divergent && let DiffScope::Change(commit_id) = &target.scope {
                    identity.scope = format!("{}:{commit_id}", identity.scope);
                }

                let matches = |view: &Self, cx: &App| {
                    view.identity == identity
                        && view.has_working_copy_buffer(cx) == historical.is_none()
                        && view.working_path(cx).is_none_or(|current| current == path)
                };
                let existing = existing_view
                    .filter(|view| view.read_with(cx, matches))
                    .or_else(|| {
                        workspace.read_with(cx, |workspace, cx| {
                            workspace
                                .items_of_type::<Self>(cx)
                                .find(|item| restored.is_none() && matches(item.read(cx), cx))
                        })
                    });
                if let Some(existing) = existing {
                    if existing.read_with(cx, |view, _| view.has_pending_classification()) {
                        workspace.update_in(cx, |workspace, window, cx| {
                            Self::show(&existing, mode, workspace, window, cx)
                        })?;
                        return Ok(existing);
                    }

                    let buffer = existing.read_with(cx, |view, _| view.buffer.clone());
                    if let Some(contents) = saved_buffer.take() {
                        buffer.update(cx, |buffer, cx| {
                            if buffer.capability() == Capability::ReadWrite && !buffer.is_dirty() {
                                contents.restore(buffer, cx);
                            }
                        });
                    }
                    if let Some(historical) = historical {
                        let edits = buffer
                            .read_with(cx, |buffer, cx| buffer.diff(historical, cx))
                            .await;
                        buffer.update(cx, |buffer, cx| {
                            buffer.apply_diff(edits, cx);
                            buffer.did_save(buffer.version(), None, cx);
                        });
                    }

                    existing.update(cx, |view, cx| {
                        view.refresh_generation += 1;
                        view.refresh_task = Task::ready(());
                        view.error = None;
                        view.snapshot = Some(snapshot_for_view);
                        view.classification = None;
                        view.preview_task = Task::ready(());
                        view.target = target;
                        view.file = file_for_view;
                        cx.emit(EditorEvent::TitleChanged);
                        view.controls.update(cx, |controls, cx| {
                            controls.status = Some(file_status(&view.file));
                            cx.notify();
                        });
                        view.base = base;
                        view.base_revision = base_revision;
                        view.update_task = view.diff.update(cx, |diff, cx| {
                            diff.set_base_text(
                                view.base.clone(),
                                buffer.read(cx).snapshot().text,
                                cx,
                            )
                        });
                        cx.notify();
                    });
                    workspace.update_in(cx, |workspace, window, cx| {
                        Self::show(&existing, mode, workspace, window, cx);
                    })?;
                    return Ok(existing);
                }

                let buffer = if let Some(text) = historical {
                    let registry = project.read_with(cx, |project, _| project.languages().clone());
                    let project_path =
                        project.read_with(cx, |project, cx| project.find_project_path(&path, cx));
                    let worktree_id = project.read_with(cx, |project, cx| {
                        project_path
                            .as_ref()
                            .map(|path| path.worktree_id)
                            .or_else(|| {
                                project
                                    .worktrees(cx)
                                    .next()
                                    .map(|worktree| worktree.read(cx).id())
                            })
                            .unwrap_or_else(|| project::WorktreeId::from_usize(0))
                    });
                    let blob = Arc::new(crate::commit_view::GitBlob {
                        path: if let Some(project_path) = project_path {
                            git::repository::RepoPath::from_rel_path(&project_path.path)
                        } else {
                            git::repository::RepoPath::new(&file_for_view.path)?
                        },
                        worktree_id,
                        is_deleted: false,
                        is_binary: presentation != DiffPresentation::Text,
                        display_name: file_for_view.path.clone(),
                    });
                    let buffer =
                        crate::commit_view::build_buffer(text, blob, &registry, cx).await?;
                    buffer.update(cx, |buffer, cx| {
                        buffer.set_capability(Capability::ReadOnly, cx);
                        if presentation != DiffPresentation::Text {
                            buffer.set_language(None, cx);
                        }
                    });
                    buffer
                } else {
                    project
                        .update(cx, |project, cx| project.open_local_buffer(path, cx))
                        .await?
                };
                if let Some(contents) = saved_buffer {
                    buffer.update(cx, |buffer, cx| {
                        if buffer.capability() == Capability::ReadWrite && !buffer.is_dirty() {
                            contents.restore(buffer, cx);
                        }
                    });
                }

                let diff = cx.new(|cx| {
                    let buffer = buffer.read(cx);
                    BufferDiff::new(
                        &buffer.snapshot().text,
                        buffer.language().cloned(),
                        buffer.language_registry(),
                        cx,
                    )
                });
                diff.update(cx, |diff, cx| {
                    diff.set_base_text(base.clone(), buffer.read(cx).snapshot().text, cx)
                })
                .await;

                let view = workspace.update_in(cx, |workspace, window, cx| {
                    let workspace_entity = cx.entity();
                    let view = cx.new(|cx| {
                        let mut view = Self::new(
                            identity,
                            target,
                            file_for_view,
                            buffer,
                            diff,
                            base,
                            project,
                            workspace_entity,
                            window,
                            cx,
                        );
                        view.base_revision = base_revision;
                        view.snapshot = Some(snapshot_for_view);
                        view
                    });
                    if let Some(saved) = restored {
                        let view = view.downgrade();
                        window.defer(cx, move |window, cx| {
                            view.update(cx, |view, cx| {
                                view.editor.update(cx, |editor, cx| {
                                    if (editor.diff_view_style() == settings::DiffViewStyle::Split)
                                        != saved.split
                                    {
                                        editor.toggle_split(&editor::ToggleSplitDiff, window, cx);
                                    }

                                    if saved.split {
                                        editor.split(window, cx);
                                    }

                                    let selected_editor = if saved.left {
                                        editor.lhs_editor().unwrap_or_else(|| editor.rhs_editor())
                                    } else {
                                        editor.rhs_editor()
                                    };
                                    selected_editor.update(cx, |editor, cx| {
                                        saved.editor.restore(editor, window, cx)
                                    });
                                });
                            })
                            .log_err();
                        });
                    } else {
                        view.update(cx, |view, cx| {
                            let editor = view.editor.read(cx).rhs_editor().clone();
                            editor.update(cx, |editor, cx| {
                                let first_change = editor
                                    .buffer()
                                    .read(cx)
                                    .snapshot(cx)
                                    .diff_hunks()
                                    .next()
                                    .map(|hunk| Point::new(hunk.row_range.start.0, 0));
                                if let Some(position) = first_change {
                                    editor.change_selections(
                                        SelectionEffects::scroll(Autoscroll::center()),
                                        window,
                                        cx,
                                        |selections| selections.select_ranges([position..position]),
                                    );
                                }
                            });
                        });
                        Self::show(&view, mode, workspace, window, cx);
                    }

                    view
                })?;

                Ok(view)
            }
            .await;
            performance.finish(
                if result.is_ok() { "completed" } else { "error" },
                serde_json::json!({"boundary": "workspace item activated"}),
            );
            result
        })
    }

    fn show(
        view: &Entity<Self>,
        mode: DiffOpenMode,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut App,
    ) {
        if mode == DiffOpenMode::Hidden {
            return;
        }
        let focus = mode == DiffOpenMode::Open;
        if let Some(pane) = workspace.pane_for_item_id(view.entity_id()) {
            if focus {
                pane.update(cx, |pane, _| {
                    pane.unpreview_item_if_preview(view.entity_id());
                });
            }

            workspace.activate_item(view, focus, focus, window, cx);
        } else {
            let pane = workspace.active_pane().clone();
            let settings = PreviewTabsSettings::get_global(cx);
            let existing = view
                .project_entry_ids(cx)
                .first()
                .and_then(|entry| pane.read(cx).item_for_entry(*entry, cx));
            let permanent = existing
                .as_ref()
                .is_some_and(|item| !pane.read(cx).is_active_preview_item(item.item_id()));
            let previous_index = existing
                .as_ref()
                .and_then(|item| pane.read(cx).index_for_item(item.as_ref()));
            let preview = mode == DiffOpenMode::Preview
                && settings.enabled
                && settings.enable_preview_from_project_panel
                && !view.read(cx).is_dirty(cx)
                && !permanent;
            let destination = if preview {
                pane.update(cx, |pane, cx| {
                    pane.replace_preview_item_id(view.entity_id(), window, cx)
                })
            } else {
                if let Some(existing) = &existing {
                    pane.update(cx, |pane, _| {
                        pane.unpreview_item_if_preview(existing.item_id())
                    });
                }

                // Native singleton insertion otherwise keeps an already-active view of this file.
                previous_index.map(|index| index + 1)
            };
            workspace.add_item(
                pane.clone(),
                Box::new(view.clone()),
                destination,
                focus,
                focus,
                window,
                cx,
            );
            if !preview && let Some(index) = previous_index {
                pane.update(cx, |pane, cx| {
                    pane.add_item(
                        Box::new(view.clone()),
                        false,
                        false,
                        Some(index),
                        window,
                        cx,
                    );
                });
            }
        }
    }

    fn new(
        identity: DiffIdentity,
        target: DiffTarget,
        file: ChangedFile,
        buffer: Entity<Buffer>,
        diff: Entity<BufferDiff>,
        base: Option<Arc<str>>,
        project: Entity<Project>,
        workspace: Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let weak_workspace = workspace.downgrade();
        let showing_full_file = EditorSettings::get_global(cx).file_diff.show_full_file;
        if identity.working_copy
            && identity.presentation == DiffPresentation::Text
            && buffer.read(cx).capability() == Capability::ReadWrite
        {
            diff.update(cx, |diff, _| {
                diff.set_operations(Arc::new(RestoreDiffOperations))
            });
        }
        let multibuffer = cx.new(|cx| {
            let (ranges, context_lines) = super::kk_diff_controls::KkDiffControls::excerpt_ranges(
                &buffer,
                &diff,
                showing_full_file,
                cx,
            );
            let mut multibuffer = MultiBuffer::without_headers(buffer.read(cx).capability());
            multibuffer.set_excerpts_for_buffer(buffer.clone(), ranges, context_lines, cx);
            multibuffer.add_diff(diff.clone(), cx);
            multibuffer.set_all_diff_hunks_expanded(cx);
            multibuffer.with_title(identity.path.clone())
        });
        let editor = cx.new(|cx| {
            let editor = SplittableEditor::new(
                EditorSettings::get_global(cx).diff_view_style,
                multibuffer,
                project,
                workspace,
                window,
                cx,
            );
            editor.rhs_editor().update(cx, |editor, cx| {
                editor.set_allow_git_diff_scrollbar_markers(showing_full_file, cx);
                editor.set_show_diff_review_button(true, cx);
            });
            editor
        });
        let controls = cx.new(|cx| {
            super::kk_diff_controls::KkDiffControls::new(
                editor.clone(),
                showing_full_file,
                weak_workspace.clone(),
                cx,
            )
        });
        controls.update(cx, |controls, cx| {
            controls.status = Some(file_status(&file));
            controls.generic_style_controls = !identity.working_copy;
            controls.set_buffers(
                vec![(
                    buffer.clone(),
                    diff.clone(),
                    identity.presentation == DiffPresentation::Text,
                )],
                cx,
            )
        });
        let subscriptions = vec![
            cx.subscribe(&editor, |this: &mut Self, _, event: &EditorEvent, cx| {
                this.attach_editor_addons(cx);
                cx.emit(event.clone())
            }),
            cx.observe(&editor, |this: &mut Self, _, cx| {
                this.attach_editor_addons(cx)
            }),
            cx.subscribe(&buffer, |this: &mut Self, _, event, cx| {
                if matches!(event, BufferEvent::Edited { .. }) {
                    let snapshot = this.buffer.read(cx).snapshot().text;
                    this.update_task = this.diff.update(cx, |diff, cx| {
                        diff.set_base_text(this.base.clone(), snapshot, cx)
                    });
                } else if matches!(event, BufferEvent::LanguageChanged(_)) {
                    this.update_base_language(cx);
                }
            }),
            cx.subscribe(&diff, |_, _, _, cx| cx.notify()),
        ];
        let mut view = Self {
            identity,
            target,
            file,
            snapshot: None,
            workspace: weak_workspace,
            buffer,
            diff,
            editor,
            controls,
            base,
            base_revision: None,
            writes: Task::ready(Ok(())).shared(),
            update_task: Task::ready(()),
            refresh_task: Task::ready(()),
            refresh_generation: 0,
            refreshing: None,
            classification_sequence: 0,
            classification: None,
            preview_task: Task::ready(()),
            error: None,
            _subscriptions: subscriptions,
        };
        view.update_base_language(cx);
        view.attach_editor_addons(cx);
        view
    }

    fn update_base_language(&mut self, cx: &mut Context<Self>) {
        if self.identity.presentation != DiffPresentation::Text {
            return;
        }

        let buffer = self.buffer.read(cx);
        let language = buffer.language().cloned();
        let registry = buffer.language_registry();
        let base = self.diff.read(cx).base_text_buffer().clone();
        let changed = base.update(cx, |base, cx| {
            let changed = base.language().cloned() != language;
            if let Some(registry) = registry {
                base.set_language_registry(registry);
            }
            base.set_language_async(language, cx);
            changed
        });
        if changed {
            self.update_task = self.diff.update(cx, |diff, cx| {
                diff.set_base_text(self.base.clone(), self.buffer.read(cx).snapshot().text, cx)
            });
        }
    }

    fn attach_editor_addons(&self, cx: &mut Context<Self>) {
        let editors = {
            let split = self.editor.read(cx);
            std::iter::once(split.rhs_editor().clone())
                .chain(split.lhs_editor().cloned())
                .collect::<Vec<_>>()
        };
        for editor in editors {
            if editor.read(cx).addon::<DiffAddon>().is_none() {
                let view = cx.weak_entity();
                editor.update(cx, |editor, _| editor.register_addon(DiffAddon(view)));
            }
        }
    }

    pub(super) fn refresh(&mut self, snapshot: Snapshot, cx: &mut Context<Self>) {
        if self
            .classification
            .as_ref()
            .is_some_and(|preview| preview.uncertain || !preview.waiting.is_empty())
            || snapshot.repository().root() != self.identity.root
            || self.snapshot.as_ref().is_some_and(|previous| {
                previous.operation() == snapshot.operation()
                    && previous.repository().check_lifetime().is_ok()
            })
        {
            return;
        }

        if self
            .refreshing
            .as_ref()
            .is_some_and(|(generation, pending)| {
                *generation == self.refresh_generation
                    && pending.operation() == snapshot.operation()
                    && pending.repository().lifetime() == snapshot.repository().lifetime()
                    && pending.repository().check_lifetime().is_ok()
            })
        {
            return;
        }

        self.refresh_generation += 1;
        let generation = self.refresh_generation;
        self.refreshing = Some((generation, snapshot.clone()));
        let preview_task = std::mem::replace(&mut self.preview_task, Task::ready(()));
        let target = self.target.clone();
        let path = self.identity.path.clone();
        let presentation = self.identity.presentation;
        let live = self.has_working_copy_buffer(cx)
            || (presentation != DiffPresentation::Text && self.identity.working_copy);
        let draft = self
            .buffer
            .read(cx)
            .is_dirty()
            .then(|| self.buffer.read(cx).text().into_bytes());
        let lifetime = snapshot.repository().lifetime().cloned();
        self.refresh_task = cx.spawn(async move |this, cx| {
            preview_task.await;
            let result = cx
                .background_spawn(async move {
                    let (target, _) = target.resolve(&snapshot).await?;
                    let file = snapshot
                        .changed_files(&target.scope)
                        .await?
                        .into_iter()
                        .find(|file| file.path == path)
                        .unwrap_or(ChangedFile {
                            path,
                            change_type: ChangeKind::Modified,
                            renamed_from: None,
                            has_conflict: false,
                        });
                    let contents = DiffContents::load(
                        &snapshot,
                        &target,
                        &file,
                        live,
                        draft,
                        Some(presentation),
                    )
                    .await?;
                    anyhow::Ok((snapshot, target, file, contents))
                })
                .await;
            let result = async {
                let (snapshot, target, file, contents) = result?;
                let edits = if let Some(text) = contents.historical {
                    let edits =
                        this.read_with(cx, |this, cx| this.buffer.read(cx).diff(text, cx))?;
                    Some(edits.await)
                } else {
                    None
                };
                this.update(cx, |this, cx| {
                    if generation != this.refresh_generation
                        || snapshot.repository().check_lifetime().is_err()
                    {
                        return;
                    }

                    if let Some(edits) = edits {
                        this.buffer.update(cx, |buffer, cx| {
                            if !buffer.is_dirty() {
                                buffer.apply_diff(edits, cx);
                                buffer.did_save(buffer.version(), None, cx);
                            }
                        });
                    }

                    this.snapshot = Some(snapshot);
                    this.classification = None;
                    this.target = target;
                    this.file = file;
                    cx.emit(EditorEvent::TitleChanged);
                    this.controls.update(cx, |controls, cx| {
                        controls.status = Some(file_status(&this.file));
                        cx.notify();
                    });
                    this.base = contents.base;
                    this.base_revision = contents.base_revision;
                    this.error = None;
                    this.update_task = this.diff.update(cx, |diff, cx| {
                        diff.set_base_text(
                            this.base.clone(),
                            this.buffer.read(cx).snapshot().text,
                            cx,
                        )
                    });
                    cx.notify();
                })?;
                anyhow::Ok(())
            }
            .await;
            this.update(cx, |this, _| {
                if generation == this.refresh_generation {
                    this.refreshing = None;
                }
            })
            .log_err();
            if let Err(error) = result {
                this.update(cx, |this, cx| {
                    if generation == this.refresh_generation
                        && lifetime
                            .as_ref()
                            .is_none_or(|lifetime| lifetime.check().is_ok())
                    {
                        this.error = Some(format!("Could not refresh diff: {error:#}"));
                        cx.notify();
                    }
                })
                .log_err();
            }
        });
    }

    fn copy_reference(&self, include_selection: bool, cx: &mut App) {
        let split = self.editor.read(cx);
        let editor = split.focused_editor().clone();
        let mut original =
            include_selection && split.lhs_editor().is_some_and(|left| left == &editor);
        let modified_buffer_id = self.buffer.read(cx).remote_id();
        let lines = if include_selection {
            editor.update(cx, |editor, cx| {
                let display = editor.display_snapshot(cx);
                let selections = editor.selections.all_adjusted(&display);
                let nonempty: Vec<_> = selections
                    .iter()
                    .filter(|selection| selection.start != selection.end)
                    .collect();
                let cursor = editor.selections.newest_adjusted(&display).head();
                if let Some((buffer, _)) = display.buffer_snapshot().point_to_buffer_point(cursor) {
                    original |= buffer.remote_id() != modified_buffer_id;
                }

                let selected_lines: Vec<_> = nonempty
                    .into_iter()
                    .filter_map(|selection| {
                        let (_, start) = display
                            .buffer_snapshot()
                            .point_to_buffer_point(selection.start)?;
                        let (_, end) = display
                            .buffer_snapshot()
                            .point_to_buffer_point(selection.end)?;
                        Some((start.row + 1, end.row + u32::from(end.column > 0)))
                    })
                    .collect();
                let cursor_row = display
                    .buffer_snapshot()
                    .point_to_buffer_point(cursor)
                    .map(|(_, point)| point.row + 1)
                    .unwrap_or(1);
                let start = selected_lines
                    .iter()
                    .map(|(start, _)| *start)
                    .min()
                    .unwrap_or(cursor_row);
                let end = selected_lines
                    .iter()
                    .map(|(_, end)| *end)
                    .max()
                    .unwrap_or(start)
                    .max(start);
                if start == end {
                    format!(":{start}")
                } else {
                    format!(":{start}~{end}")
                }
            })
        } else {
            String::new()
        };
        let path = if original {
            self.file.renamed_from.as_deref().unwrap_or(&self.file.path)
        } else {
            &self.identity.path
        };
        let reference = format!("{path}{lines} ({})", self.target.reference);
        if let Some(snapshot) = &self.snapshot {
            snapshot.repository().diagnostics().debug("copydiff", || {
                format!(
                    "[COPY_DIFF] {}",
                    serde_json::json!({
                        "file": path, "side": if original {
                            "original"
                        } else {
                            "modified"
                        },
                        "selection": include_selection, "reference": reference,
                    })
                )
            });
        }

        cx.write_to_clipboard(ClipboardItem::new_string(reference));
    }

    pub(super) fn open_file_in_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(snapshot) = self.snapshot.clone() else {
            return;
        };
        let panel = self
            .workspace
            .upgrade()
            .and_then(|workspace| workspace.read(cx).panel::<super::kk_panel::KkPanel>(cx));
        if let Some(panel) = panel {
            panel.update(cx, |panel, cx| {
                panel.open_plain_file(
                    snapshot,
                    self.target.clone(),
                    self.file.clone(),
                    self.identity.working_copy,
                    super::kk_panel::FileOpenKind::Current,
                    window,
                    cx,
                )
            });
        }
    }

    pub fn reveal_line(&self, line: &ChangedLine, window: &mut Window, cx: &mut App) {
        let split = self.editor.read(cx);
        let left = (line.kind == LineKind::Removed)
            .then(|| split.lhs_editor().cloned())
            .flatten();
        let editor = left.clone().unwrap_or_else(|| split.rhs_editor().clone());
        let base = self.diff.read(cx).base_text_buffer().clone();
        let source = if line.kind == LineKind::Removed {
            base.clone()
        } else {
            self.buffer.clone()
        };
        let point = Point::new(line.line_number.saturating_sub(1), 0);
        editor.update(cx, |editor, cx| {
            let multibuffer = editor.buffer().read(cx);
            let anchor = if line.kind == LineKind::Removed && left.is_none() {
                multibuffer
                    .buffer_point_to_anchor(
                        &self.buffer,
                        Point::new(line.new_line_number.saturating_sub(1), 0),
                        cx,
                    )
                    .map(|anchor| {
                        anchor
                            .bias_left(&multibuffer.snapshot(cx))
                            .with_diff_base_anchor(base.read(cx).snapshot().anchor_before(point))
                    })
            } else {
                multibuffer.buffer_point_to_anchor(&source, point, cx)
            };
            if let Some(anchor) = anchor {
                editor.change_selections(
                    editor::SelectionEffects::scroll(editor::scroll::Autoscroll::center()),
                    window,
                    cx,
                    |selections| {
                        selections.select_ranges([anchor..anchor]);
                    },
                );
                window.focus(&editor.focus_handle(cx), cx);
            }
        });
    }

    fn classification_ranges(&self, cx: &mut App) -> Result<Vec<LineRange>> {
        ensure!(
            self.identity.presentation == DiffPresentation::Text,
            "Use the file list to classify binary or file-information changes"
        );
        ensure!(
            !self.buffer.read(cx).is_dirty(),
            "Save the file before classifying its changes"
        );
        let main_snapshot = self.buffer.read(cx).snapshot();
        let base_snapshot = self.diff.read(cx).base_text_buffer().read(cx).snapshot();
        let editor = self.editor.read(cx).focused_editor().clone();
        let (ranges, cursor) = editor.update(cx, |editor, cx| {
            let display = editor.display_snapshot(cx);
            let mut ranges = Vec::new();
            for selection in editor.selections.all_adjusted(&display) {
                if selection.start == selection.end {
                    continue;
                }

                for (buffer, range, _) in display
                    .buffer_snapshot()
                    .range_to_buffer_ranges_with_deleted_hunks(selection.range())
                {
                    if range.start == range.end {
                        continue;
                    }

                    let start = range.start.0.to_point(buffer);
                    let end = range.end.0.to_point(buffer);
                    ranges.push(LineRange {
                        side: if buffer.remote_id() == main_snapshot.remote_id() {
                            SelectionSide::Modified
                        } else {
                            SelectionSide::Original
                        },
                        begin: start.row + 1,
                        end: end.row + u32::from(end.column > 0),
                    });
                }
            }

            let cursor = editor.selections.newest_adjusted(&display).head();
            let cursor = display
                .buffer_snapshot()
                .point_to_buffer_point(cursor)
                .map(|(buffer, point)| (buffer.remote_id() == main_snapshot.remote_id(), point));
            (ranges, cursor)
        });
        if !ranges.is_empty() {
            return Ok(ranges);
        }

        let (modified, cursor) = cursor.context("The cursor does not belong to this file")?;
        let end = if modified {
            main_snapshot.max_point()
        } else {
            base_snapshot.max_point()
        };
        let line_count = end.row + u32::from(end.column > 0);
        let diff = self.diff.read(cx).snapshot(cx);
        ensure!(
            diff.buffer_version() == main_snapshot.version(),
            "The diff is still updating. Retry after it finishes."
        );
        for hunk in diff.hunks(&main_snapshot.text) {
            let original = hunk.diff_base_byte_range.start.to_point(&base_snapshot)
                ..hunk.diff_base_byte_range.end.to_point(&base_snapshot);
            let range = if modified { &hunk.range } else { &original };
            let last_row = range.end.row + u32::from(range.end.column > 0);
            let contains = if cursor.row == line_count && last_row == line_count {
                true
            } else if range.is_empty() {
                cursor.row == range.start.row.min(line_count.saturating_sub(1))
            } else {
                cursor.row >= range.start.row && cursor.row < last_row
            };
            if !contains {
                continue;
            }

            let (side, range) = if !range.is_empty() {
                (
                    if modified {
                        SelectionSide::Modified
                    } else {
                        SelectionSide::Original
                    },
                    range,
                )
            } else if modified {
                (SelectionSide::Original, &original)
            } else {
                (SelectionSide::Modified, &hunk.range)
            };
            let begin = range.start.row + 1;
            let end = range.end.row + u32::from(range.end.column > 0);
            ensure!(begin <= end, "There are no changed lines at the cursor");
            return Ok(vec![LineRange { side, begin, end }]);
        }

        anyhow::bail!("There are no changed lines at the cursor")
    }

    pub(super) fn matches_classification(
        &self,
        snapshot: &Snapshot,
        target: &DiffTarget,
        file: &ChangedFile,
    ) -> bool {
        self.identity.presentation == DiffPresentation::Text
            && self.identity.root == snapshot.repository().root()
            && self.target.identity == target.identity
            && self.file.path == file.path
            && self
                .snapshot
                .as_ref()
                .is_some_and(|current| current.operation() == snapshot.operation())
    }

    pub(super) fn preview_whole_file(
        &mut self,
        direction: TransferDirection,
        cx: &mut Context<Self>,
    ) -> Result<super::kk_classification::QueuedPreview> {
        ensure!(
            !self.buffer.read(cx).is_dirty(),
            "Save the file before classifying its changes"
        );
        ensure!(
            direction != TransferDirection::Forward || !self.has_working_copy_buffer(cx),
            "Only historical category diffs can preview a forward transfer"
        );
        let mut preview = match &self.classification {
            Some(preview) => preview.clone(),
            None => PendingClassification {
                model: {
                    let model = ClassificationPreview::for_whole_file(
                        &self.diff.read(cx).base_text_buffer().read(cx).text(),
                        &self.buffer.read(cx).text(),
                    );
                    cx.new(|_| model)
                },
                waiting: HashSet::new(),
                latest: None,
                uncertain: false,
                uncertain_ids: HashSet::new(),
            },
        };
        ensure!(
            !preview.uncertain,
            "Refresh KK Workbench to confirm the previous operation"
        );
        self.classification_sequence += 1;
        let id = self.classification_sequence;
        let mut model = preview.model.read(cx).clone();
        model.add(id, LineSelection::whole_file(), direction)?;
        preview.model.update(cx, |preview, _| *preview = model);
        preview.waiting.insert(id);
        let queued = super::kk_classification::QueuedPreview {
            model: preview.model.clone(),
            id,
            file: self.file.clone(),
        };
        self.classification = Some(preview);
        self.refresh_generation += 1;
        self.refresh_task = Task::ready(());
        self.render_classification_preview(cx);
        Ok(queued)
    }

    fn preview_classification(
        &mut self,
        ranges: &[LineRange],
        direction: TransferDirection,
        cx: &mut Context<Self>,
    ) -> Result<(u64, Vec<LineRange>)> {
        let original = self.diff.read(cx).base_text_buffer().read(cx).text();
        let modified = self.buffer.read(cx).text();
        ensure!(
            direction != TransferDirection::Forward || !self.has_working_copy_buffer(cx),
            "Only historical category diffs can preview a forward transfer"
        );
        let mut preview = match &self.classification {
            Some(preview) => preview.clone(),
            None => PendingClassification {
                model: {
                    let model = ClassificationPreview::new(&original, &modified)?;
                    cx.new(|_| model)
                },
                waiting: HashSet::new(),
                latest: None,
                uncertain: false,
                uncertain_ids: HashSet::new(),
            },
        };
        ensure!(
            !preview.uncertain,
            "Refresh KK Workbench to confirm the previous operation"
        );
        let mut model = preview.model.read(cx).clone();
        let side = ranges.first().context("No changed lines selected")?.side;
        let mut anchors = Vec::new();
        for range in ranges {
            ensure!(
                range.begin > 0 && range.begin <= range.end,
                "Invalid line selection"
            );
            let selection = model.capture(
                &original,
                &modified,
                &LineSelection::ranges(
                    range.side,
                    vec![usize::try_from(range.begin - 1)?..usize::try_from(range.end)?],
                ),
            )?;
            anchors.extend(selection.anchors.unwrap_or_default());
        }

        self.classification_sequence += 1;
        let id = self.classification_sequence;
        model.add(id, LineSelection::anchored(side, anchors), direction)?;
        let ranges = model.native_ranges(id)?;
        ensure!(
            !ranges.is_empty(),
            "The selection does not contain unclassified changed lines"
        );
        preview.waiting.insert(id);
        preview.model.update(cx, |preview, _| *preview = model);
        self.classification = Some(preview);
        self.refresh_generation += 1;
        self.refresh_task = Task::ready(());
        self.render_classification_preview(cx);
        Ok((id, ranges))
    }

    fn render_classification_preview(&mut self, cx: &mut Context<Self>) {
        if let Some(preview) = &self.classification {
            self.base = Some(Arc::from(preview.model.read(cx).text()));
            if !self.has_working_copy_buffer(cx)
                && self.buffer.read(cx).text() != preview.model.read(cx).modified_text()
            {
                let edits = self
                    .buffer
                    .read(cx)
                    .diff(preview.model.read(cx).modified_text().to_owned(), cx);
                self.preview_task = cx.spawn(async move |this, cx| {
                    let edits = edits.await;
                    this.update(cx, |this, cx| {
                        this.buffer.update(cx, |buffer, cx| {
                            buffer.apply_diff(edits, cx);
                            buffer.did_save(buffer.version(), None, cx);
                        });
                        this.update_task = this.diff.update(cx, |diff, cx| {
                            diff.set_base_text(
                                this.base.clone(),
                                this.buffer.read(cx).snapshot().text,
                                cx,
                            )
                        });
                        cx.notify();
                    })
                    .log_err();
                });
            } else {
                self.preview_task = Task::ready(());
            }

            self.update_task = self.diff.update(cx, |diff, cx| {
                diff.set_base_text(self.base.clone(), self.buffer.read(cx).snapshot().text, cx)
            });
            cx.notify();
        }
    }

    pub(super) fn finish_classification_preview(
        &mut self,
        id: u64,
        result: Result<Snapshot, super::kk_classification::ClassificationFailure>,
        cx: &mut Context<Self>,
    ) {
        let Some(preview) = &mut self.classification else {
            return;
        };
        preview.waiting.remove(&id);
        match result {
            Ok(snapshot) => {
                if preview
                    .latest
                    .as_ref()
                    .is_none_or(|(previous, _)| *previous < id)
                {
                    preview.latest = Some((id, snapshot));
                }
            }
            Err(error) => {
                preview.uncertain |= error.uncertain;
                if error.uncertain {
                    preview.uncertain_ids.insert(id);
                }

                self.error = Some(error.message);
                if !error.uncertain
                    && let Err(error) = preview.model.update(cx, |preview, _| preview.remove(id))
                {
                    self.error = Some(format!(
                        "Could not restore the classification preview: {error:#}"
                    ));
                }
            }
        }

        let finished = preview.waiting.is_empty();
        let uncertain = preview.uncertain;
        let latest = preview
            .latest
            .as_ref()
            .map(|(_, snapshot)| snapshot.clone());
        self.render_classification_preview(cx);
        if finished && !uncertain {
            if let Some(snapshot) = latest {
                cx.spawn(async move |this, cx| {
                    let result = cx
                        .background_spawn(
                            async move { snapshot.repository().snapshot(false).await },
                        )
                        .await;
                    this.update(cx, |this, cx| match result {
                        Ok(snapshot) => this.refresh(snapshot, cx),
                        Err(error) => {
                            this.error =
                                Some(format!("Could not refresh classification: {error:#}"));
                            cx.notify();
                        }
                    })
                    .log_err();
                })
                .detach();
            } else {
                self.classification = None;
            }
        }

        cx.notify();
    }

    pub(super) fn resolve_classification_uncertainty(
        &mut self,
        snapshot: &Snapshot,
        applied: bool,
        cx: &mut Context<Self>,
    ) {
        if snapshot.repository().root() != self.identity.root {
            return;
        }

        let Some(preview) = &mut self.classification else {
            return;
        };
        if !preview.uncertain {
            return;
        }

        preview.uncertain = false;
        self.error = None;
        for id in preview.uncertain_ids.drain() {
            if applied {
                if preview
                    .latest
                    .as_ref()
                    .is_none_or(|(previous, _)| *previous < id)
                {
                    preview.latest = Some((id, snapshot.clone()));
                }
            } else if let Err(error) = preview.model.update(cx, |model, _| model.remove(id)) {
                self.error = Some(format!(
                    "Could not restore the rejected classification preview: {error:#}"
                ));
            }
        }

        let finished = preview.waiting.is_empty();
        self.render_classification_preview(cx);
        if finished {
            self.classification = None;
            self.refresh(snapshot.clone(), cx);
        }

        cx.notify();
    }

    fn classify(&mut self, category: ClassificationTarget, cx: &mut Context<Self>) {
        let result = (|| {
            let intent = self
                .target
                .comparison
                .clone()
                .context("Open a comparison range before classifying changes")?;
            let snapshot = self.snapshot.clone().context("The diff is still loading")?;
            let ranges = self.classification_ranges(cx)?;
            let workspace = self.workspace.upgrade().context("Workspace was closed")?;
            let panel = workspace
                .read(cx)
                .panel::<super::kk_panel::KkPanel>(cx)
                .context("KK Workbench is unavailable")?;
            let direction =
                panel
                    .read(cx)
                    .classification_direction(&snapshot, &self.target, category);
            let (preview, ranges) = if let Some(direction) = direction {
                let (id, ranges) = self.preview_classification(&ranges, direction, cx)?;
                (Some(id), ranges)
            } else {
                (None, ranges)
            };
            let task = panel.update(cx, |panel, cx| {
                panel.queue_classification(
                    snapshot,
                    self.target.clone(),
                    intent,
                    vec![FileSelection {
                        file: self.file.clone(),
                        ranges: Some(ranges),
                    }],
                    category,
                    preview
                        .and_then(|id| {
                            self.classification.as_ref().map(|preview| {
                                super::kk_classification::QueuedPreview {
                                    model: preview.model.clone(),
                                    id,
                                    file: self.file.clone(),
                                }
                            })
                        })
                        .into_iter()
                        .collect(),
                    cx,
                )
            });
            cx.spawn(async move |this, cx| {
                let result = task.await;
                this.update(cx, |this, cx| {
                    if let Some(id) = preview {
                        this.finish_classification_preview(id, result, cx);
                    } else if let Err(error) = result {
                        this.error = Some(error.message);
                        cx.notify();
                    }
                })
                .log_err();
            })
            .detach();
            anyhow::Ok(())
        })();
        self.error = result.err().map(|error| format!("{error:#}"));
        cx.notify();
    }

    fn search_query(&self, window: &mut Window, cx: &mut App) -> Option<String> {
        let query = self
            .editor
            .update(cx, |editor, cx| {
                editor.query_suggestion(Some(settings::SeedQuerySetting::Always), window, cx)
            })
            .replace("\r\n", "\n");
        (!query.is_empty()).then_some(query)
    }

    fn search_selection(&self, window: &mut Window, cx: &mut App) {
        let Some(snapshot) = self.snapshot.clone() else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let Some(query) = self.search_query(window, cx) else {
            return;
        };
        let target = self.target.clone();
        window.defer(cx, move |window, cx| {
            super::kk_search::DiffSearch::open(
                snapshot,
                target,
                Some(query),
                workspace,
                window,
                cx,
            );
        });
    }
}

impl EventEmitter<EditorEvent> for KkDiff {}

impl Focusable for KkDiff {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Render for KkDiff {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.controls.update(cx, |controls, cx| {
            if let Some(snapshot) = self.snapshot.clone() {
                controls.set_context(snapshot, self.target.clone(), self.workspace.clone(), cx);
            }
        });
        v_flex()
            .size_full()
            .on_action(cx.listener(|this, _: &CopyDiff, _, cx| this.copy_reference(true, cx)))
            .on_action(cx.listener(|this, _: &OpenFileInProject, window, cx| {
                this.open_file_in_project(window, cx)
            }))
            .on_action(cx.listener(|this, _: &MarkPending, _, cx| {
                this.classify(ClassificationTarget::Category(Category::Pending), cx)
            }))
            .on_action(cx.listener(|this, _: &MarkReviewed, _, cx| {
                this.classify(ClassificationTarget::Category(Category::Reviewed), cx)
            }))
            .on_action(cx.listener(|this, _: &MarkRejected, _, cx| {
                this.classify(ClassificationTarget::Category(Category::Rejected), cx)
            }))
            .on_action(cx.listener(|this, _: &MarkDebug, _, cx| {
                this.classify(ClassificationTarget::Category(Category::Debug), cx)
            }))
            .on_action(cx.listener(|this, _: &ReturnToComparison, _, cx| {
                this.classify(ClassificationTarget::Comparison, cx)
            }))
            .on_action(cx.listener(
                |this, _: &super::kk_search::SearchSelectionInChanges, window, cx| {
                    this.search_selection(window, cx)
                },
            ))
            .when_some(self.error.clone(), |view, error| {
                view.child(Label::new(error).color(Color::Error))
            })
            .child(self.controls.clone())
            .child(div().flex_1().min_h_0().child(self.editor.clone()))
    }
}

impl Item for KkDiff {
    type Event = EditorEvent;

    fn can_split(&self) -> bool {
        !self.identity.working_copy && !self.has_pending_classification()
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
        let Some(file) = self.buffer.read(cx).file().cloned() else {
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
        let text = self.buffer.read(cx).text();
        let registry = project.read(cx).languages().clone();
        let identity = self.identity.clone();
        let target = self.target.clone();
        let changed_file = self.file.clone();
        let base = self.base.clone();
        let base_revision = self.base_revision.clone();
        let snapshot = self.snapshot.clone();
        let style = self.editor.read(cx).diff_view_style();
        let full_file = self.controls.read(cx).showing_full_file;
        let layout = self.controls.read(cx).capture_excerpt_layout(cx);
        let text_presentation = self.identity.presentation == DiffPresentation::Text;
        let selected = self.editor.read(cx).rhs_editor().clone();
        let state = selected.update(cx, SerializedEditorState::capture);
        cx.spawn_in(window, async move |_, cx| {
            let result = async {
                let buffer = super::commit_view::build_buffer(text, file, &registry, cx).await?;
                buffer.update(cx, |buffer, cx| {
                    buffer.set_capability(Capability::ReadOnly, cx);
                    if !text_presentation {
                        buffer.set_language(None, cx);
                    }
                });
                let diff = cx.new(|cx| {
                    let buffer = buffer.read(cx);
                    BufferDiff::new(
                        &buffer.snapshot().text,
                        buffer.language().cloned(),
                        buffer.language_registry(),
                        cx,
                    )
                });
                diff.update(cx, |diff, cx| {
                    diff.set_base_text(base.clone(), buffer.read(cx).snapshot().text, cx)
                })
                .await;
                let cloned_buffer_id = buffer.read_with(cx, |buffer, _| buffer.remote_id());
                cx.update(|window, cx| {
                    cx.new(|cx| {
                        let mut view = Self::new(
                            identity,
                            target,
                            changed_file,
                            buffer,
                            diff,
                            base,
                            project,
                            workspace,
                            window,
                            cx,
                        );
                        view.snapshot = snapshot;
                        view.base_revision = base_revision;
                        view.controls.update(cx, |controls, cx| {
                            controls.set_full_file(full_file, cx);
                            controls.restore_excerpt_layout(
                                layout
                                    .into_iter()
                                    .map(|(_, ranges)| (cloned_buffer_id, ranges))
                                    .collect(),
                                cx,
                            );
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
                    })
                })
            }
            .await;
            result.log_err()
        })
    }

    fn tab_content_text(&self, _: usize, _: &App) -> SharedString {
        self.identity.title(&self.target, &self.file)
    }
    fn tab_tooltip_text(&self, _: &App) -> Option<SharedString> {
        Some(
            format!(
                "{} ({})",
                self.identity.root.join(&self.file.path).display(),
                self.target.tab_label()
            )
            .into(),
        )
    }
    fn tab_icon(&self, _: &Window, cx: &App) -> Option<Icon> {
        Some(
            FileIcons::get_icon(std::path::Path::new(&self.file.path), cx)
                .map(Icon::from_path)
                .unwrap_or_else(|| Icon::new(IconName::File))
                .color(Color::Muted),
        )
    }
    fn to_item_events(event: &EditorEvent, callback: &mut dyn FnMut(ItemEvent)) {
        Editor::to_item_events(event, callback);
    }
    fn deactivated(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editor
            .read(cx)
            .rhs_editor()
            .clone()
            .update(cx, |editor, cx| editor.deactivated(window, cx));
    }
    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("KK Diff Opened")
    }
    fn act_as_type<'a>(
        &'a self,
        type_id: TypeId,
        handle: &'a Entity<Self>,
        cx: &'a App,
    ) -> Option<gpui::AnyEntity> {
        if type_id == TypeId::of::<Self>() {
            Some(handle.clone().into())
        } else if type_id == TypeId::of::<SplittableEditor>() {
            (!self.identity.working_copy).then(|| self.editor.clone().into())
        } else if type_id == TypeId::of::<super::kk_diff_controls::KkDiffControls>() {
            Some(self.controls.clone().into())
        } else if type_id == TypeId::of::<Editor>() {
            Some(self.editor.read(cx).rhs_editor().clone().into())
        } else {
            None
        }
    }
    fn as_searchable(&self, _: &Entity<Self>, _: &App) -> Option<Box<dyn SearchableItemHandle>> {
        Some(Box::new(self.editor.clone()))
    }
    fn can_save(&self, cx: &App) -> bool {
        self.editor.read(cx).can_save(cx)
    }
    fn buffer_kind(&self, _: &App) -> ItemBufferKind {
        // Excerpts render the diff, but the item edits one file.
        ItemBufferKind::Singleton
    }
    fn active_project_path(&self, cx: &App) -> Option<project::ProjectPath> {
        self.editor.read(cx).active_project_path(cx)
    }
    fn breadcrumb_location(&self, _: &App) -> workspace::ToolbarItemLocation {
        workspace::ToolbarItemLocation::PrimaryLeft
    }
    fn breadcrumbs(
        &self,
        cx: &App,
    ) -> Option<(Vec<language::HighlightedText>, Option<gpui::Font>)> {
        let path = git::repository::RepoPath::new(&self.file.path).ok()?;
        let text = path
            .as_ref()
            .display(util::paths::PathStyle::local())
            .into_owned()
            .into();
        Some(super::solo_diff_view::diff_file_breadcrumbs(
            text,
            Some(file_status(&self.file)),
            cx,
        ))
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
        data: Arc<dyn std::any::Any + Send>,
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
        self.buffer.read(cx).capability()
    }
    fn has_deleted_file(&self, cx: &App) -> bool {
        self.editor.read(cx).has_deleted_file(cx)
    }
    fn has_conflict(&self, cx: &App) -> bool {
        self.editor.read(cx).has_conflict(cx)
    }
    fn reload(
        &mut self,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.editor
            .update(cx, |editor, cx| editor.reload(project, window, cx))
    }
    fn is_dirty(&self, cx: &App) -> bool {
        self.editor.read(cx).is_dirty(cx)
    }
    fn preserve_preview(&self, cx: &App) -> bool {
        let buffer = self.buffer.read(cx);
        buffer.capability() == Capability::ReadOnly || buffer.preserve_preview()
    }
    fn save(
        &mut self,
        options: SaveOptions,
        project: Entity<Project>,
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
    fn for_each_project_item(
        &self,
        cx: &App,
        callback: &mut dyn FnMut(gpui::EntityId, &dyn project::ProjectItem),
    ) {
        self.editor.read(cx).for_each_project_item(cx, callback);
    }
}

impl SerializableItem for KkDiff {
    fn serialized_item_kind() -> &'static str {
        "KKDiff"
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
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        workspace_id: WorkspaceId,
        item_id: ItemId,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Entity<Self>>> {
        let store = KeyValueStore::global(cx);
        let namespace = session_namespace(workspace_id);
        let host = project
            .read(cx)
            .remote_connection_options(cx)
            .map(|options| options.host_key());
        let remote = project.read(cx).kk_workbench_client(cx);
        let executable = super::kk_settings::KkWorkbenchSettings::get_global(cx)
            .executable_path
            .clone();
        let restore_unsaved = project::project_settings::ProjectSettings::get_global(cx)
            .session
            .restore_unsaved_buffers;
        window.spawn(cx, async move |cx| {
            let (snapshot, saved) = cx
                .background_spawn(async move {
                    let value = store
                        .scoped(&namespace)
                        .read(&item_id.to_string())?
                        .context("Saved KK diff is unavailable")?;
                    let mut saved: SavedDiffTab = serde_json::from_str(&value)?;
                    ensure!(
                        saved.host == host,
                        "This diff belongs to another project host"
                    );
                    if !restore_unsaved {
                        saved.buffer = None;
                    }

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
                    anyhow::Ok((snapshot, saved))
                })
                .await?;
            cx.update(|window, cx| {
                Self::load(
                    snapshot,
                    saved.target.clone(),
                    saved.file.clone(),
                    saved.working_copy,
                    workspace,
                    DiffLoadState {
                        saved_tab: Some(saved),
                        ..DiffLoadState::default()
                    },
                    DiffOpenMode::Open,
                    window,
                    cx,
                )
            })?
            .await
        })
    }

    fn serialize(
        &mut self,
        workspace: &mut Workspace,
        item_id: ItemId,
        _: bool,
        cx: &mut Context<Self>,
    ) -> Option<Task<Result<()>>> {
        self.snapshot.as_ref()?;
        let namespace = session_namespace(workspace.database_id()?);
        let project = workspace.project().clone();
        let store = KeyValueStore::global(cx);
        let previous = self.writes.clone();
        let owner = cx.entity();
        self.writes = cx
            .spawn(async move |this, cx| {
                let _owner = owner;
                if let Err(error) = previous.await {
                    log::warn!("Earlier KK diff persistence failed: {error}");
                }

                let saved = this
                    .update(cx, |this, cx| {
                        let restore_unsaved =
                            project::project_settings::ProjectSettings::get_global(cx)
                                .session
                                .restore_unsaved_buffers;
                        let buffer = this.buffer.read(cx);
                        let buffer = (restore_unsaved
                            && buffer.capability() == Capability::ReadWrite
                            && buffer.is_dirty())
                        .then(|| SerializedBufferContents::capture(buffer));
                        let editor = this.editor.read(cx);
                        let split = editor.diff_view_style() == settings::DiffViewStyle::Split;
                        let selected_editor = editor.focused_editor().clone();
                        let left = editor.lhs_editor() == Some(&selected_editor);
                        anyhow::Ok(SavedDiffTab {
                            root: this.identity.root.clone(),
                            target: this.target.clone(),
                            file: this.file.clone(),
                            working_copy: this.identity.working_copy,
                            operation: this
                                .snapshot
                                .as_ref()
                                .context("The diff is still loading")?
                                .operation()
                                .to_owned(),
                            host: project
                                .read(cx)
                                .remote_connection_options(cx)
                                .map(|options| options.host_key()),
                            buffer,
                            split,
                            left,
                            editor: selected_editor.update(cx, SerializedEditorState::capture),
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
                .map_err(|error| format!("Could not restore this diff next session: {error:#}"))
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
mod tests {
    use super::*;
    use crate::kk_panel::remote_tests::run_kk;
    use editor::{
        ToPoint as _, display_map::DisplayRow, test::editor_lsp_test_context::EditorLspTestContext,
    };
    use futures::StreamExt as _;
    use gpui::{BorrowAppContext, TestAppContext, VisualContext};
    use project::{FakeFs, Fs};
    use serde_json::json;
    use settings::{DiffViewStyle, SettingsStore};
    use std::path::Path;
    use workspace::{MultiWorkspace, ToolbarItemView};

    #[gpui::test]
    async fn kk_conflict_comparison_uses_shared_controls(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings = SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/repo", json!({})).await;
        let project = Project::test(fs, [Path::new("/repo")], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |view, _| view.workspace().clone());
        let prefix = "unchanged\n".repeat(100);
        for (base, text) in [
            (format!("{prefix}old\n"), format!("{prefix}new\n")),
            (String::new(), "added\n".into()),
            ("removed\n".into(), String::new()),
            (String::new(), String::new()),
            ("same\n".into(), "same\n".into()),
        ] {
            let base: Arc<str> = base.into();
            let buffer = cx.new(|cx| {
                let mut buffer = Buffer::local(text, cx);
                buffer.set_capability(Capability::ReadOnly, cx);
                buffer
            });
            let diff = cx.new(|cx| {
                let buffer = buffer.read(cx);
                BufferDiff::new(
                    &buffer.snapshot().text,
                    None,
                    buffer.language_registry(),
                    cx,
                )
            });
            diff.update(cx, |diff, cx| {
                diff.set_base_text(Some(base.clone()), buffer.read(cx).snapshot().text, cx)
            })
            .await;
            let view = cx.new_window_entity(|window, cx| {
                KkDiff::conflict_comparison(
                    "Base 1 ↔ Side 1".into(),
                    "file.txt".into(),
                    buffer,
                    diff,
                    base,
                    project.clone(),
                    workspace.clone(),
                    window,
                    cx,
                )
            });
            view.update_in(cx, |view, window, cx| {
                assert!(view.snapshot.is_none());
                assert_eq!(view.capability(cx), Capability::ReadOnly);
                assert!(!view.can_save(cx));
                let editor = view.editor.read(cx).rhs_editor().clone();
                editor.update(cx, |editor, cx| {
                    let snapshot = editor.buffer().read(cx).snapshot(cx);
                    if let Some(first) = snapshot.diff_hunks().next() {
                        assert_eq!(
                            editor
                                .selections
                                .newest::<Point>(&editor.display_snapshot(cx))
                                .head()
                                .row,
                            first.row_range.start.0
                        );
                    }
                });
                view.controls.update(cx, |controls, cx| {
                    assert!(controls.generic_style_controls);
                    controls.set_full_file(true, cx);
                });
                view.editor.update(cx, |editor, cx| {
                    editor.toggle_split(&editor::ToggleSplitDiff, window, cx);
                });
                view.controls
                    .update(cx, |controls, cx| controls.set_full_file(false, cx));
                assert!(!view.is_dirty(cx));
            });
        }
    }

    #[test]
    fn kk_diff_titles_match_reference_revision_labels() {
        let mut target = DiffTarget::comparison(
            "klmnopqrstuv".into(),
            "base".into(),
            "klmnopqrstuv description".into(),
            Some("qrstuvwxyzkl"),
        );
        let identity = DiffIdentity {
            root: "/repository".into(),
            scope: target.identity.clone(),
            path: "nested/foo.rs".into(),
            working_copy: true,
            presentation: DiffPresentation::Text,
        };
        let mut file = ChangedFile {
            path: identity.path.clone(),
            change_type: ChangeKind::Modified,
            renamed_from: None,
            has_conflict: false,
        };
        assert_eq!(
            identity.title(&target, &file).as_ref(),
            "foo.rs (qrstuvwx::@)"
        );
        target
            .comparison
            .as_mut()
            .expect("comparison")
            .range_start_change_id = None;
        assert_eq!(
            identity.title(&target, &file).as_ref(),
            "foo.rs (@ vs klmnopqr)"
        );
        target.comparison = None;
        target.scope = DiffScope::Change("commit".into());
        target.label = "klmnopqrstuv".into();
        assert_eq!(identity.title(&target, &file).as_ref(), "foo.rs (klmnopqr)");
        file.change_type = ChangeKind::Added;
        assert_eq!(
            identity.title(&target, &file).as_ref(),
            "foo.rs (added in klmnopqr)"
        );
        file.change_type = ChangeKind::Deleted;
        assert_eq!(
            identity.title(&target, &file).as_ref(),
            "foo.rs (removed in klmnopqr)"
        );
        target.label = "abc123".into();
        file.change_type = ChangeKind::Renamed;
        assert_eq!(identity.title(&target, &file).as_ref(), "foo.rs (abc123)");
        target.label = "prefix_klmnopqrstuv".into();
        assert_eq!(target.tab_label(), "prefix_klmnopqrstuv");
    }

    #[gpui::test(iterations = 3)]
    async fn kk_diff_reveals_changes_and_tracks_delayed_language(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let (directory, snapshot, target, file, modified) = cx
            .background_spawn(async {
                let directory = tempfile::tempdir()?;
                let root = directory.path();
                run_kk(root, &["init", "."]).await?;
                run_kk(
                    root,
                    &["config", "set", "--repo", "user.name", "KK Diff Test"],
                )
                .await?;
                run_kk(
                    root,
                    &[
                        "config",
                        "set",
                        "--repo",
                        "user.email",
                        "test@example.invalid",
                    ],
                )
                .await?;
                let prefix = (0..80)
                    .map(|line| format!("// unchanged {line}\n"))
                    .collect::<String>();
                let removed = "pub fn removed() {\n    let deleted = 1;\n}\n";
                let gap = "// unchanged after deletion\n".repeat(20);
                let original =
                    format!("{prefix}{removed}{gap}pub fn changed() {{\n    let value = 1;\n}}\n");
                let modified = original
                    .replace(removed, "")
                    .replace("value = 1", "value = 2");
                std::fs::write(root.join("submodule.rs"), original)?;
                run_kk(root, &["new", "-m", "change"]).await?;
                std::fs::write(root.join("submodule.rs"), &modified)?;
                let repository = kk_workbench::Repository::open(root, Path::new("kk")).await?;
                let snapshot = repository.snapshot(true).await?;
                let revision = snapshot.exact_revision("@").await?;
                let target = DiffTarget::change(&revision);
                let file = snapshot
                    .changed_files(&target.scope)
                    .await?
                    .into_iter()
                    .next()
                    .context("changed Rust file")?;
                anyhow::Ok((directory, snapshot, target, file, modified))
            })
            .await
            .expect("prepare Rust diff");
        cx.update(|cx| {
            let mut store = SettingsStore::test(cx);
            store
                .set_user_settings(
                    r#"{"diff_view_style":"split","minimum_split_diff_width":20,"git":{"disable_git":true}}"#,
                    cx,
                )
                .expect("split settings");
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(directory.path(), json!({"submodule.rs": modified}))
            .await;
        let project = Project::test(fs, [directory.path()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |view, _| view.workspace().clone());
        install_diff_toolbars(&workspace, cx);
        let view = cx
            .update(|window, cx| {
                KkDiff::open(
                    snapshot.clone(),
                    target.clone(),
                    file.clone(),
                    true,
                    workspace.downgrade(),
                    DiffOpenMode::Preview,
                    window,
                    cx,
                )
            })
            .await
            .expect("open Rust diff");
        let (buffer, base_buffer, editor) = view.read_with(cx, |view, cx| {
            (
                view.buffer.clone(),
                view.diff.read(cx).base_text_buffer().clone(),
                view.editor.clone(),
            )
        });
        assert!(buffer.read_with(cx, |buffer, _| buffer.language().is_none()));
        let (first_change, cursor_row) = editor.update_in(cx, |editor, window, cx| {
            editor.split(window, cx);
            let left = editor.lhs_editor().expect("left diff editor");
            left.update(cx, |left, cx| {
                assert!(
                    left.snapshot(window, cx)
                        .display_snapshot
                        .row_infos(DisplayRow(0))
                        .any(|row| row.buffer_row == Some(80)
                            && row.diff_status.is_some_and(|status| status.is_deleted()))
                );
            });
            editor.rhs_editor().update(cx, |right, cx| {
                let snapshot = right.snapshot(window, cx);
                assert!(
                    snapshot
                        .display_snapshot
                        .row_infos(DisplayRow(0))
                        .any(|row| row.diff_status.is_some_and(|status| status.is_added()))
                );
                let first_change = snapshot
                    .buffer_snapshot()
                    .diff_hunks()
                    .next()
                    .expect("visible diff hunk")
                    .row_range
                    .start
                    .0;
                assert!(first_change >= 80);
                let cursor_row = right
                    .selections
                    .newest::<Point>(&snapshot.display_snapshot)
                    .head()
                    .row;
                (first_change, cursor_row)
            })
        });
        editor.read_with(cx, |editor, cx| {
            assert!(
                editor
                    .rhs_editor()
                    .read(cx)
                    .git_diff_scrollbar_markers_enabled(cx)
            );
        });
        let rust = language::rust_lang();
        project
            .read_with(cx, |project, _| project.languages().clone())
            .add(rust.clone());
        rust.set_theme(&theme::SyntaxTheme::new([(
            "keyword".into(),
            gpui::rgba(0x000000ff).into(),
        )]));
        buffer.update(cx, |buffer, cx| buffer.set_language(Some(rust), cx));
        cx.run_until_parked();
        let base_has_rust = base_buffer.read_with(cx, |buffer, _| {
            buffer
                .language()
                .is_some_and(|language| language.name().as_ref() == "Rust")
        });
        assert!(
            base_has_rust && cursor_row == first_change,
            "base Rust language: {base_has_rust}; initial cursor row: {cursor_row}; first change: {first_change}"
        );
        base_buffer.read_with(cx, |buffer, _| {
            let snapshot = buffer.snapshot();
            assert!(
                snapshot
                    .chunks(
                        0..snapshot.len(),
                        language::LanguageAwareStyling {
                            tree_sitter: true,
                            diagnostics: false
                        }
                    )
                    .any(|chunk| chunk.syntax_highlight_id.is_some())
            );
        });
        assert_diff_navigation_controls(&editor, cx);
        editor.update_in(cx, |editor, window, cx| {
            editor.toggle_split(&editor::ToggleSplitDiff, window, cx);
            editor.rhs_editor().update(cx, |right, cx| {
                assert!(right.git_diff_scrollbar_markers_enabled(cx));
                let snapshot = right.snapshot(window, cx);
                let rows = snapshot
                    .display_snapshot
                    .row_infos(DisplayRow(0))
                    .filter_map(|row| row.diff_status)
                    .collect::<Vec<_>>();
                assert!(rows.iter().any(|status| status.is_added()));
                assert!(rows.iter().any(|status| status.is_deleted()));
                let deleted = snapshot
                    .buffer_snapshot()
                    .diff_hunks()
                    .next()
                    .expect("deleted hunk");
                let deleted_range = Point::new(deleted.row_range.start.0, 0)
                    ..Point::new(deleted.row_range.end.0, 0);
                assert!(
                    snapshot
                        .buffer_snapshot()
                        .chunks(
                            deleted_range,
                            language::LanguageAwareStyling {
                                tree_sitter: true,
                                diagnostics: false
                            }
                        )
                        .any(|chunk| chunk.syntax_highlight_id.is_some()),
                    "inline deleted text has Rust syntax highlighting"
                );
            });
        });
        assert_diff_navigation_controls(&editor, cx);
        editor.update_in(cx, |editor, window, cx| {
            editor.rhs_editor().update(cx, |right, cx| {
                right.change_selections(
                    editor::SelectionEffects::no_scroll(),
                    window,
                    cx,
                    |selections| selections.select_ranges([Point::new(5, 0)..Point::new(5, 0)]),
                );
            });
        });
        cx.update(|window, cx| {
            KkDiff::open(
                snapshot,
                target,
                file,
                true,
                workspace.downgrade(),
                DiffOpenMode::Preview,
                window,
                cx,
            )
        })
        .await
        .expect("reopen Rust diff");
        editor.update_in(cx, |editor, window, cx| {
            editor.rhs_editor().update(cx, |right, cx| {
                let snapshot = right.snapshot(window, cx);
                assert_eq!(
                    right
                        .selections
                        .newest::<Point>(&snapshot.display_snapshot)
                        .head()
                        .row,
                    5
                );
            });
        });
    }

    fn assert_diff_navigation_controls(
        editor: &Entity<SplittableEditor>,
        cx: &mut gpui::VisualTestContext,
    ) {
        let (first, next) = editor.update_in(cx, |editor, window, cx| {
            editor.rhs_editor().update(cx, |right, cx| {
                let snapshot = right.snapshot(window, cx);
                let mut hunks = snapshot.buffer_snapshot().diff_hunks();
                let first = hunks.next().expect("first change").row_range.start.0;
                let next = hunks.next().expect("second change").row_range.start.0;
                (first, next)
            })
        });
        for (selector, expected) in [("ICON-ArrowDown", next), ("ICON-ArrowUp", first)] {
            cx.update(|window, cx| {
                window.refresh();
                window.draw(cx).clear(cx);
            });
            let bounds = cx
                .debug_bounds(selector)
                .expect("native diff navigation button");
            cx.simulate_click(bounds.center(), gpui::Modifiers::none());
            cx.run_until_parked();
            editor.update_in(cx, |editor, window, cx| {
                editor.rhs_editor().update(cx, |right, cx| {
                    let snapshot = right.snapshot(window, cx);
                    assert_eq!(
                        right
                            .selections
                            .newest::<Point>(&snapshot.display_snapshot)
                            .head()
                            .row,
                        expected,
                        "{selector}"
                    );
                });
            });
        }
    }

    #[gpui::test(iterations = 3)]
    async fn kk_diff_history_details_and_pane_clone(cx: &mut TestAppContext) {
        let directory = super::super::kk_multi_diff::tests::local_fixture(cx).await;
        let root = directory.path().join("repo");
        let original = (0..80)
            .map(|line| format!("line {line}\n"))
            .collect::<String>();
        std::fs::write(root.join("a.txt"), &original).expect("context base");
        run_kk(&root, &["new", "-m", "context base"])
            .await
            .expect("freeze context base");
        let modified = original.replace("line 60\n", "changed 60\n");
        std::fs::write(root.join("a.txt"), &modified).expect("later hunk");
        std::fs::write(root.join("binary.rs"), b"binary\0content").expect("binary source filename");
        run_kk(
            &root,
            &["describe", "-m", "Diff subject\n\nFull **description**"],
        )
        .await
        .expect("description");
        run_kk(&root, &["new", "-m", "next"])
            .await
            .expect("freeze revision");
        let repository = kk_workbench::Repository::open(&root, Path::new("kk"))
            .await
            .expect("repository");
        let snapshot = repository.snapshot(true).await.expect("snapshot");
        let revision = snapshot
            .exact_revision("@-")
            .await
            .expect("historical revision");
        assert!(revision.commit_timestamp.is_some());
        let target = DiffTarget::change(&revision);
        let file = snapshot
            .changed_files(&target.scope)
            .await
            .expect("files")
            .into_iter()
            .find(|file| file.path == "a.txt")
            .expect("file");
        cx.update(|cx| {
            editor::init(cx);
            crate::init(cx);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            &root,
            json!({"a.txt":modified,"binary.rs":"binary\0content"}),
        )
        .await;
        let project = Project::test(fs, [root.as_path()], cx).await;
        project
            .read_with(cx, |project, _| project.languages().clone())
            .add(language::rust_lang());
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = multi_workspace.read_with(cx, |view, _| view.workspace().clone());
        let _search = install_diff_toolbars(&workspace, cx);
        let panel = cx
            .update(|window, cx| {
                super::super::kk_panel::KkPanel::load(workspace.downgrade(), window.to_async(cx))
            })
            .await
            .expect("KK panel");
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_panel(panel, window, cx)
        });
        let view = cx
            .update(|window, cx| {
                KkDiff::open(
                    snapshot.clone(),
                    target.clone(),
                    file,
                    false,
                    workspace.downgrade(),
                    DiffOpenMode::Open,
                    window,
                    cx,
                )
            })
            .await
            .expect("history diff");
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("commit-details").is_some());
        assert!(
            cx.debug_bounds("multibuffer-collapse-expand-empty")
                .is_none()
        );
        assert!(cx.debug_bounds("ICON-Eye").is_some());
        let toggle = cx
            .debug_bounds("ICON-Eye")
            .expect("commit information toggle");
        cx.simulate_click(toggle.center(), gpui::Modifiers::none());
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("commit-details").is_none());
        assert!(cx.debug_bounds("ICON-Eye").is_some());
        let toggle = cx
            .debug_bounds("ICON-Eye")
            .expect("commit information toggle");
        cx.simulate_click(toggle.center(), gpui::Modifiers::none());
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("commit-details").is_some());
        assert!(cx.debug_bounds("ICON-MagnifyingGlass").is_some());
        assert!(cx.debug_bounds("ICON-GitGraph").is_some());
        view.read_with(cx, |view, cx| {
            assert!(view.can_split());
            assert!(view.breadcrumbs(cx).is_some());
            assert!(
                !view
                    .editor
                    .read(cx)
                    .rhs_editor()
                    .read(cx)
                    .buffer()
                    .read(cx)
                    .snapshot(cx)
                    .show_headers()
            );
            assert_eq!(view.buffer.read(cx).capability(), Capability::ReadOnly);
            assert_eq!(
                view.target
                    .revision
                    .as_ref()
                    .expect("details")
                    .full_description,
                revision.full_description
            );
        });
        let selected = view.update_in(cx, |view, window, cx| {
            view.controls.update(cx, |controls, cx| {
                if controls.showing_full_file {
                    controls.toggle_full_file(cx);
                }
            });
            let right = view.editor.read(cx).rhs_editor().clone();
            right.update(cx, |right, cx| {
                let multibuffer = right.buffer().clone();
                multibuffer.update(cx, |multibuffer, cx| {
                    multibuffer.set_excerpts_for_buffer(
                        view.buffer.clone(),
                        [Point::new(10, 0)..view.buffer.read(cx).max_point()],
                        0,
                        cx,
                    );
                });
                let anchor = multibuffer
                    .read(cx)
                    .buffer_point_to_anchor(&view.buffer, Point::new(60, 0), cx)
                    .expect("selected logical line");
                right.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
                    selections.select_ranges([anchor..anchor])
                });
                let snapshot = right.snapshot(window, cx);
                right
                    .selections
                    .newest::<Point>(&snapshot.display_snapshot)
                    .head()
            })
        });
        let clone = view
            .update_in(cx, |view, window, cx| view.clone_on_split(None, window, cx))
            .await
            .expect("pane clone");
        cx.run_until_parked();
        clone.update(cx, |view, cx| {
            view.controls
                .update(cx, |controls, cx| controls.toggle_commit_details(cx));
        });
        view.read_with(cx, |view, cx| {
            assert!(!view.controls.read(cx).showing_commit_details(cx));
            assert!(!clone.read(cx).controls.read(cx).showing_commit_details(cx));
        });
        view.update(cx, |view, cx| {
            view.controls
                .update(cx, |controls, cx| controls.toggle_commit_details(cx));
        });
        view.read_with(cx, |view, cx| {
            let clone = clone.read(cx);
            assert_eq!(
                view.controls
                    .read(cx)
                    .capture_excerpt_layout(cx)
                    .into_iter()
                    .map(|(_, ranges)| ranges)
                    .collect::<Vec<_>>(),
                clone
                    .controls
                    .read(cx)
                    .capture_excerpt_layout(cx)
                    .into_iter()
                    .map(|(_, ranges)| ranges)
                    .collect::<Vec<_>>()
            );
            assert_eq!(
                clone
                    .editor
                    .read(cx)
                    .rhs_editor()
                    .read(cx)
                    .git_diff_scrollbar_markers_enabled(cx),
                clone.controls.read(cx).showing_full_file
            );
            assert_ne!(view.editor, clone.editor);
            assert_ne!(view.buffer, clone.buffer);
            assert_eq!(view.buffer.read(cx).text(), clone.buffer.read(cx).text());
            assert_eq!(
                view.diff.read(cx).changed_row_counts(),
                clone.diff.read(cx).changed_row_counts()
            );
            assert_eq!(
                view.target
                    .revision
                    .as_ref()
                    .expect("details")
                    .full_commit_id,
                clone
                    .target
                    .revision
                    .as_ref()
                    .expect("cloned details")
                    .full_commit_id
            );
        });
        clone.update_in(cx, |view, window, cx| {
            view.editor
                .read(cx)
                .rhs_editor()
                .clone()
                .update(cx, |right, cx| {
                    let snapshot = right.snapshot(window, cx);
                    assert_eq!(
                        right
                            .selections
                            .newest::<Point>(&snapshot.display_snapshot)
                            .head(),
                        selected
                    );
                })
        });
        let binary = snapshot
            .changed_files(&target.scope)
            .await
            .expect("binary files")
            .into_iter()
            .find(|file| file.path == "binary.rs")
            .expect("binary file");
        let information = cx
            .update(|window, cx| {
                KkDiff::open(
                    snapshot,
                    target,
                    binary,
                    false,
                    workspace.downgrade(),
                    DiffOpenMode::Open,
                    window,
                    cx,
                )
            })
            .await
            .expect("metadata page");
        assert!(information.read_with(cx, |view, cx| view.buffer.read(cx).language().is_none()));
        let copy = information
            .update_in(cx, |view, window, cx| view.clone_on_split(None, window, cx))
            .await
            .expect("metadata pane clone");
        assert!(copy.read_with(cx, |view, cx| view.buffer.read(cx).language().is_none()));
    }

    fn install_diff_toolbars(
        workspace: &Entity<Workspace>,
        cx: &mut gpui::VisualTestContext,
    ) -> Entity<search::BufferSearchBar> {
        workspace.update_in(cx, |workspace, window, cx| {
            let toolbar = workspace.active_pane().read(cx).toolbar().clone();
            toolbar.update(cx, |toolbar, cx| {
                let style = cx.new(super::super::solo_diff_view::SoloDiffStyleToolbar::new);
                toolbar.add_item(style, window, cx);
                let search = cx.new(|cx| {
                    search::BufferSearchBar::new(
                        Some(workspace.project().read(cx).languages().clone()),
                        window,
                        cx,
                    )
                });
                toolbar.add_item(search.clone(), window, cx);
                let actions = cx.new(super::super::solo_diff_view::SoloDiffGitToolbar::new);
                toolbar.add_item(actions, window, cx);
                search
            })
        })
    }

    async fn assert_diff_navigation_history(
        workspace: &Entity<Workspace>,
        editor: &Entity<SplittableEditor>,
        buffer: &Entity<Buffer>,
        cx: &mut gpui::VisualTestContext,
    ) {
        editor.update_in(cx, |editor, window, cx| {
            if editor.diff_view_style() != DiffViewStyle::Split {
                editor.toggle_split(&editor::ToggleSplitDiff, window, cx);
            }
            editor.split(window, cx);
        });
        cx.update(|window, cx| {
            window.refresh();
            window.draw(cx).clear(cx);
        });
        editor.update_in(cx, |editor, window, cx| {
            editor.rhs_editor().update(cx, |right, cx| {
                window.focus(&right.focus_handle(cx), cx);
                for row in [20, 60] {
                    let anchor = right
                        .buffer()
                        .read(cx)
                        .buffer_point_to_anchor(buffer, Point::new(row, 0), cx)
                        .expect("diff navigation destination");
                    right.change_selections(
                        SelectionEffects::no_scroll().nav_history(true),
                        window,
                        cx,
                        |selections| selections.select_ranges([anchor..anchor]),
                    );
                }
            });
        });
        for (forward, expected_row) in [(false, 20), (true, 60)] {
            editor.update_in(cx, |editor, window, cx| {
                let left = editor.lhs_editor().expect("split navigation editor");
                window.focus(&left.focus_handle(cx), cx);
            });
            cx.update(|window, cx| window.draw(cx).clear(cx));
            editor.read_with(cx, |editor, _| {
                assert_eq!(editor.focused_editor(), editor.lhs_editor().expect("left"));
            });
            workspace
                .update_in(cx, |workspace, window, cx| {
                    let pane = workspace.active_pane().downgrade();
                    if forward {
                        workspace.go_forward(pane, window, cx)
                    } else {
                        workspace.go_back(pane, window, cx)
                    }
                })
                .await
                .expect("diff navigation history");
            cx.update(|window, cx| window.draw(cx).clear(cx));
            editor.update_in(cx, |editor, window, cx| {
                assert_eq!(editor.focused_editor(), editor.rhs_editor());
                editor.rhs_editor().update(cx, |right, cx| {
                    let snapshot = right.snapshot(window, cx);
                    let point = right
                        .selections
                        .newest::<Point>(&snapshot.display_snapshot)
                        .head();
                    assert_eq!(
                        snapshot
                            .buffer_snapshot()
                            .point_to_buffer_point(point)
                            .map(|(_, point)| point.row),
                        Some(expected_row)
                    );
                });
            });
        }
    }

    #[gpui::test(iterations = 3)]
    async fn kk_diff_native_controls_restore_and_multifile(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let (directory, snapshot, target, files, original, modified) = cx
            .background_spawn(async {
                let directory = tempfile::tempdir()?;
                let root = directory.path();
                run_kk(root, &["init", "."]).await?;
                run_kk(
                    root,
                    &["config", "set", "--repo", "user.name", "KK Diff Test"],
                )
                .await?;
                run_kk(
                    root,
                    &[
                        "config",
                        "set",
                        "--repo",
                        "user.email",
                        "test@example.invalid",
                    ],
                )
                .await?;
                let original = (0..80)
                    .map(|line| format!("line {line}\n"))
                    .collect::<String>();
                let modified = original
                    .replace("line 20\n", "changed 20\n")
                    .replace("line 60\n", "changed 60\n");
                std::fs::write(root.join("one.txt"), &original)?;
                std::fs::write(root.join("two.txt"), "old\n")?;
                run_kk(root, &["new", "-m", "changes"]).await?;
                std::fs::write(root.join("one.txt"), &modified)?;
                std::fs::write(root.join("two.txt"), "new\n")?;
                std::fs::write(root.join("added.txt"), "new file\n")?;
                let repository = kk_workbench::Repository::open(root, Path::new("kk")).await?;
                let snapshot = repository.snapshot(true).await?;
                let target = DiffTarget::change(&snapshot.exact_revision("@").await?);
                let files = snapshot.changed_files(&target.scope).await?;
                anyhow::Ok((directory, snapshot, target, files, original, modified))
            })
            .await
            .expect("prepare native diff controls");
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            crate::init(cx);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            directory.path(),
            json!({"one.txt": modified, "two.txt": "new\n", "added.txt": "new file\n"}),
        )
        .await;
        let project = Project::test(fs.clone(), [directory.path()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |view, _| view.workspace().clone());
        let search_bar = install_diff_toolbars(&workspace, cx);
        let first = files
            .iter()
            .find(|file| file.path == "one.txt")
            .expect("first file")
            .clone();
        let single = cx
            .update(|window, cx| {
                KkDiff::open(
                    snapshot.clone(),
                    target.clone(),
                    first,
                    true,
                    workspace.downgrade(),
                    DiffOpenMode::Open,
                    window,
                    cx,
                )
            })
            .await
            .expect("single diff");
        let (editor, controls, buffer) = single.read_with(cx, |view, _| {
            (
                view.editor.clone(),
                view.controls.clone(),
                view.buffer.clone(),
            )
        });
        assert_eq!(
            search_bar.update_in(cx, |search, window, cx| {
                search.set_active_pane_item(Some(&single), window, cx)
            }),
            workspace::ToolbarItemLocation::Hidden
        );
        editor.update_in(cx, |editor, window, cx| {
            editor.rhs_editor().update(cx, |right, cx| {
                right.change_selections(
                    editor::SelectionEffects::no_scroll(),
                    window,
                    cx,
                    |selections| {
                        selections.select_ranges([Point::new(5, 0)..Point::new(6, 0)]);
                    },
                );
                assert!(!right.can_restore_selected_hunks(window, cx));
            });
        });
        single.read_with(cx, |view, cx| {
            let (crumbs, _) = view.breadcrumbs(cx).expect("native file breadcrumb");
            assert_eq!(
                crumbs.first().map(|crumb| crumb.text.as_ref()),
                Some("one.txt")
            );
        });
        editor.update_in(cx, |editor, window, cx| {
            editor.rhs_editor().update(cx, |right, cx| {
                right.show_diff_review_overlay(DisplayRow(5)..DisplayRow(5), window, cx);
                let prompt = right
                    .diff_review_prompt_editor()
                    .expect("review input")
                    .clone();
                prompt.update(cx, |prompt, cx| {
                    prompt.insert("Keep this context", window, cx)
                });
            });
        });
        cx.update(|window, cx| {
            window.refresh();
            window.draw(cx).clear(cx);
        });
        assert!(cx.debug_bounds("solo-diff-style-controls").is_some());
        let toggle = cx
            .debug_bounds("ICON-ChevronDownUp")
            .expect("native Show Changes Only control");
        cx.simulate_click(toggle.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(!controls.read_with(cx, |controls, _| controls.showing_full_file));
        editor.update_in(cx, |editor, window, cx| {
            editor.rhs_editor().update(cx, |right, cx| {
                let prompt = right
                    .diff_review_prompt_editor()
                    .expect("retained review input")
                    .clone();
                assert_eq!(prompt.read(cx).text(cx), "Keep this context");
                window.focus(&prompt.focus_handle(cx), cx);
                right.submit_diff_review_comment(window, cx);
            });
        });
        editor.read_with(cx, |editor, cx| {
            assert_eq!(
                editor.rhs_editor().read(cx).diff_review_text(cx),
                "one.txt:6-6\nKeep this context\n\n"
            );
        });
        editor.read_with(cx, |editor, cx| {
            let snapshot = editor.rhs_editor().read(cx).buffer().read(cx).snapshot(cx);
            assert_eq!(snapshot.total_changed_lines(), (2, 2));
            assert!(snapshot.max_point().row < 40);
            assert!(
                snapshot
                    .row_infos(multi_buffer::MultiBufferRow(0))
                    .any(|row| row.expand_info.is_some())
            );
        });
        for style in [DiffViewStyle::Split, DiffViewStyle::Unified] {
            editor.update_in(cx, |editor, window, cx| {
                if editor.diff_view_style() != style {
                    editor.toggle_split(&editor::ToggleSplitDiff, window, cx);
                }
                if style == DiffViewStyle::Split {
                    editor.split(window, cx);
                }
                let source = if style == DiffViewStyle::Split {
                    single.read(cx).diff.read(cx).base_text_buffer().clone()
                } else {
                    buffer.clone()
                };
                let selected = if style == DiffViewStyle::Split {
                    editor.lhs_editor().expect("left diff editor")
                } else {
                    editor.rhs_editor()
                };
                selected.update(cx, |right, cx| {
                    let anchor = right
                        .buffer()
                        .read(cx)
                        .buffer_point_to_anchor(&source, Point::new(20, 0), cx)
                        .expect("changed line");
                    right.change_selections(
                        SelectionEffects::no_scroll(),
                        window,
                        cx,
                        |selections| selections.select_ranges([anchor..anchor]),
                    );
                    window.focus(&right.focus_handle(cx), cx);
                });
            });
            cx.update(|window, cx| window.draw(cx).clear(cx));
            let cursor = editor.read_with(cx, |editor, cx| {
                let selected = if style == DiffViewStyle::Split {
                    editor.lhs_editor().expect("rendered left editor")
                } else {
                    editor.rhs_editor()
                };
                selected
                    .read(cx)
                    .pixel_position_of_cursor(cx)
                    .expect("visible restore selection")
            });
            cx.simulate_click(cursor, gpui::Modifiers::none());
            editor.update_in(cx, |editor, window, cx| {
                let selected = if style == DiffViewStyle::Split {
                    editor.lhs_editor().expect("rendered left editor")
                } else {
                    editor.rhs_editor()
                };
                selected.update(cx, |selected, cx| {
                    assert!(
                        selected.focus_handle(cx).is_focused(window),
                        "{style:?} restore focus"
                    );
                    let snapshot = selected.snapshot(window, cx);
                    let head = selected
                        .selections
                        .newest::<Point>(&snapshot.display_snapshot)
                        .head();
                    assert_eq!(
                        snapshot
                            .buffer_snapshot()
                            .point_to_buffer_point(head)
                            .map(|(_, point)| point.row),
                        Some(20),
                        "{style:?} restore selection"
                    );
                    assert!(
                        snapshot
                            .buffer_snapshot()
                            .diff_hunks_in_range(head..head)
                            .next()
                            .is_some(),
                        "{style:?} selected hunk"
                    );
                });
            });
            cx.dispatch_action(git::Restore);
            cx.run_until_parked();
            assert_eq!(
                buffer.read_with(cx, |buffer, _| buffer.text()),
                modified.replace("changed 20\n", "line 20\n"),
                "{style:?} restore"
            );
            editor.update_in(cx, |editor, window, cx| {
                editor.rhs_editor().update(cx, |right, cx| {
                    right.undo(&editor::actions::Undo, window, cx)
                })
            });
            cx.run_until_parked();
            assert_eq!(buffer.read_with(cx, |buffer, _| buffer.text()), modified);
        }
        editor.update_in(cx, |editor, window, cx| {
            editor.rhs_editor().update(cx, |right, cx| {
                let snapshot = right.buffer().read(cx).snapshot(cx);
                right.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
                    selections.select_ranges([Point::zero()..snapshot.max_point()])
                });
                window.focus(&right.focus_handle(cx), cx);
            });
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.dispatch_action(git::Restore);
        cx.run_until_parked();
        assert_eq!(buffer.read_with(cx, |buffer, _| buffer.text()), original);
        assert!(single.read_with(cx, |view, cx| view.can_save(cx)));
        single
            .update_in(cx, |view, window, cx| {
                view.save(SaveOptions::default(), project.clone(), window, cx)
            })
            .await
            .expect("save the last restored hunk");
        assert_eq!(
            fs.load(&directory.path().join("one.txt"))
                .await
                .expect("restored file"),
            original
        );
        controls.update(cx, |controls, cx| controls.toggle_full_file(cx));
        editor.read_with(cx, |editor, cx| {
            assert!(
                editor
                    .rhs_editor()
                    .read(cx)
                    .buffer()
                    .read(cx)
                    .snapshot(cx)
                    .max_point()
                    .row
                    >= 80
            )
        });
        controls.update(cx, |controls, cx| controls.toggle_full_file(cx));
        assert!(!controls.read_with(cx, |controls, _| controls.showing_full_file));
        editor.read_with(cx, |editor, cx| {
            assert_eq!(
                editor.rhs_editor().read(cx).diff_review_text(cx),
                "one.txt:6-6\nKeep this context\n\n"
            );
        });
        editor.update_in(cx, |editor, window, cx| {
            editor.rhs_editor().update(cx, |right, cx| {
                right.undo(&editor::actions::Undo, window, cx)
            })
        });
        cx.run_until_parked();
        assert_eq!(buffer.read_with(cx, |buffer, _| buffer.text()), modified);
        assert_diff_navigation_history(&workspace, &editor, &buffer, cx).await;
        let multi = cx
            .update(|window, cx| {
                super::super::kk_multi_diff::KkMultiDiff::open(
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
            .expect("multi-file diff");
        cx.condition(&multi, |view, _| !view.loading).await;
        let editor = cx
            .read(|cx| (&multi as &dyn workspace::ItemHandle).act_as::<SplittableEditor>(cx))
            .expect("native multi-file editor");
        editor.read_with(cx, |editor, cx| {
            let snapshot = editor.rhs_editor().read(cx).buffer().read(cx).snapshot(cx);
            assert_eq!(snapshot.all_buffer_ids().count(), 3);
            assert_eq!(snapshot.total_changed_lines(), (4, 3));
        });
        assert_eq!(
            search_bar.update_in(cx, |search, window, cx| {
                search.set_active_pane_item(Some(&multi), window, cx)
            }),
            workspace::ToolbarItemLocation::PrimaryLeft
        );
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("solo-diff-style-controls").is_none());
        assert_diff_navigation_history(&workspace, &editor, &buffer, cx).await;
        let controls = cx
            .read(|cx| {
                (&multi as &dyn workspace::ItemHandle)
                    .act_as::<super::super::kk_diff_controls::KkDiffControls>(cx)
            })
            .expect("aggregate controls");
        controls.update_in(cx, |controls, window, cx| controls.restore_all(window, cx));
        cx.run_until_parked();
        assert_eq!(buffer.read_with(cx, |buffer, _| buffer.text()), original);
        editor.read_with(cx, |editor, cx| {
            let buffers = editor.rhs_editor().read(cx).buffer().read(cx).all_buffers();
            let texts = buffers
                .iter()
                .map(|buffer| buffer.read(cx).text())
                .collect::<Vec<_>>();
            assert!(texts.iter().any(|text| text == "old\n"));
            assert!(texts.iter().any(|text| text == "new file\n"));
        });
        editor.update_in(cx, |editor, window, cx| {
            editor.rhs_editor().update(cx, |right, cx| {
                right.undo(&editor::actions::Undo, window, cx)
            })
        });
        cx.run_until_parked();
        assert_eq!(buffer.read_with(cx, |buffer, _| buffer.text()), modified);
        let same = cx
            .update(|window, cx| {
                super::super::kk_multi_diff::KkMultiDiff::open(
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
            .expect("reopen same aggregate");
        cx.condition(&same, |view, _| !view.loading).await;
        assert_eq!(same, multi);
        assert_eq!(
            workspace.read_with(cx, |workspace, cx| workspace
                .items_of_type::<super::super::kk_multi_diff::KkMultiDiff>(cx)
                .count()),
            1
        );
        assert_ne!(original, modified);
    }

    #[gpui::test]
    async fn kk_merge_diff_loads_historical_and_editable_contents(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let (directory, snapshot, target, file) = cx
            .background_spawn(async {
                let directory = tempfile::tempdir()?;
                let root = directory.path();
                run_kk(root, &["init", "."]).await?;
                run_kk(
                    root,
                    &["config", "set", "--repo", "user.name", "KK Diff Test"],
                )
                .await?;
                run_kk(
                    root,
                    &[
                        "config",
                        "set",
                        "--repo",
                        "user.email",
                        "test@example.invalid",
                    ],
                )
                .await?;
                let repository = kk_workbench::Repository::open(root, Path::new("kk")).await?;
                std::fs::write(root.join("file.txt"), "base\n")?;
                std::fs::write(root.join("binary.bin"), b"old\0binary")?;
                run_kk(root, &["new", "-m", "left"]).await?;
                std::fs::write(root.join("left.txt"), "left\n")?;
                let left = repository.snapshot(true).await?.exact_revision("@").await?;
                run_kk(root, &["new", "@-", "-m", "right"]).await?;
                std::fs::write(root.join("right.txt"), "right\n")?;
                repository.snapshot(true).await?;
                run_kk(root, &["new", "@", &left.full_commit_id, "-m", "merge"]).await?;
                std::fs::write(root.join("file.txt"), "merged\n")?;
                std::fs::write(root.join("binary.bin"), b"new\0binary")?;
                let snapshot = repository.snapshot(true).await?;
                let revision = snapshot.exact_revision("@").await?;
                assert_eq!(revision.parent_commit_ids.len(), 2);
                let target = DiffTarget::change(&revision);
                let files = snapshot.changed_files(&target.scope).await?;
                let file = files
                    .iter()
                    .find(|file| file.path == "file.txt")
                    .context("changed text")?;
                let historical =
                    DiffContents::load(&snapshot, &target, file, false, None, None).await?;
                assert_eq!(historical.base.as_deref(), Some("base\n"));
                assert_eq!(historical.historical.as_deref(), Some("merged\n"));
                assert!(historical.presentation == DiffPresentation::Text);
                assert!(historical.base_revision.is_none());
                let editable = DiffContents::load(
                    &snapshot,
                    &target,
                    file,
                    true,
                    Some(b"unsaved\n".to_vec()),
                    None,
                )
                .await?;
                assert_eq!(editable.base.as_deref(), Some("base\n"));
                assert!(editable.historical.is_none());
                assert!(editable.presentation == DiffPresentation::Text);
                assert!(editable.base_revision.is_none());
                let binary = files
                    .iter()
                    .find(|file| file.path == "binary.bin")
                    .context("changed binary")?;
                let contents =
                    DiffContents::load(&snapshot, &target, binary, false, None, None).await?;
                assert!(contents.presentation == DiffPresentation::FileInformation);
                assert!(
                    contents
                        .base
                        .as_deref()
                        .is_some_and(|text| text.contains("binary contents")),
                    "{:?}",
                    contents.base
                );
                assert!(
                    contents
                        .historical
                        .as_deref()
                        .is_some_and(|text| text.contains("Exists: Yes"))
                );

                let file = file.clone();
                anyhow::Ok((directory, snapshot, target, file))
            })
            .await
            .expect("merge diff contents");
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(directory.path(), json!({"file.txt": "merged\n"}))
            .await;
        let project = Project::test(fs, [directory.path()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |view, _| view.workspace().clone());
        let view = cx
            .update(|window, cx| {
                KkDiff::open(
                    snapshot,
                    target,
                    file,
                    false,
                    workspace.downgrade(),
                    DiffOpenMode::Preview,
                    window,
                    cx,
                )
            })
            .await
            .expect("open merge diff");
        view.update_in(cx, |view, window, cx| {
            view.editor
                .update(cx, |editor, cx| editor.split(window, cx));
            assert!(view.editor.read(cx).lhs_editor().is_some());
            let targets = view.blame_targets(cx);
            assert_eq!(targets.len(), 1);
            assert_eq!(
                targets.first().map(|(editor, _, _)| editor),
                Some(view.editor.read(cx).rhs_editor())
            );
            assert_eq!(view.buffer.read(cx).text(), "merged\n");
        });
    }

    #[gpui::test(iterations = 3)]
    async fn kk_diff_preserves_native_language_services_and_editing_in_both_modes(
        cx: &mut TestAppContext,
    ) {
        cx.update(|cx| {
            let mut settings = SettingsStore::test(cx);
            settings
                .set_user_settings(
                    r#"{"diff_view_style":"split","minimum_split_diff_width":20,"completions":{"words":"disabled"}}"#,
                    cx,
                )
                .expect("diff settings");
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            init(cx);
        });
        let mut test = EditorLspTestContext::new_rust(
            lsp::ServerCapabilities {
                completion_provider: Some(lsp::CompletionOptions::default()),
                definition_provider: Some(lsp::OneOf::Left(true)),
                text_document_sync: Some(lsp::TextDocumentSyncCapability::Kind(
                    lsp::TextDocumentSyncKind::INCREMENTAL,
                )),
                ..Default::default()
            },
            cx,
        )
        .await;
        test.set_state("fn main() { tarˇ }\n");
        let ordinary_editor = test.editor.clone();
        let buffer = test.update_editor(|editor, _, cx| {
            editor
                .buffer()
                .read(cx)
                .as_singleton()
                .expect("project buffer")
        });
        let project = test.update_workspace(|workspace, _, _| workspace.project().clone());
        let workspace = test.workspace.clone();
        let source_uri = test.buffer_lsp_url.clone();
        let target_path = EditorLspTestContext::root_path().join("dir/definition.rs");
        let fs = project.read_with(&test.cx.cx, |project, _| project.fs().clone());
        fs.as_fake()
            .insert_file(&target_path, b"pub fn target() {}\n".to_vec())
            .await;
        let server = test.lsp;
        let cx = &mut test.cx.cx;
        install_diff_toolbars(&workspace, cx);
        let base = Some(Arc::<str>::from("fn main() {}\n"));
        let diff = cx.new(|cx| {
            BufferDiff::new(
                &buffer.read(cx).snapshot().text,
                buffer.read(cx).language().cloned(),
                buffer.read(cx).language_registry(),
                cx,
            )
        });
        diff.update(cx, |diff, cx| {
            diff.set_base_text(base.clone(), buffer.read(cx).snapshot().text, cx)
        })
        .await;
        let view = cx.new_window_entity(|window, cx| {
            KkDiff::new(
                DiffIdentity {
                    root: EditorLspTestContext::root_path().to_owned(),
                    scope: "change".into(),
                    path: "dir/file.rs".into(),
                    working_copy: true,
                    presentation: DiffPresentation::Text,
                },
                DiffTarget {
                    scope: DiffScope::Change("captured".into()),
                    identity: "change".into(),
                    label: "change".into(),
                    reference: "kkkkkkkk".into(),
                    comparison: None,
                    revision: None,
                    category: None,
                },
                ChangedFile {
                    path: "dir/file.rs".into(),
                    change_type: ChangeKind::Modified,
                    renamed_from: None,
                    has_conflict: false,
                },
                buffer.clone(),
                diff,
                base,
                project.clone(),
                workspace.clone(),
                window,
                cx,
            )
        });
        workspace.update_in(cx, |workspace, window, cx| {
            KkDiff::show(&view, DiffOpenMode::Open, workspace, window, cx)
        });
        let editor = view.read_with(cx, |view, cx| view.editor.read(cx).rhs_editor().clone());
        editor.update(cx, |editor, _| {
            editor.set_show_completions_on_input(Some(false))
        });
        let mut completions = server.set_request_handler::<lsp::request::Completion, _, _>({
            let source_uri = source_uri.clone();
            move |params, _| {
                assert_eq!(params.text_document_position.text_document.uri, source_uri);
                assert_eq!(
                    params.text_document_position.position,
                    lsp::Position::new(0, 15)
                );
                async {
                    Ok(Some(lsp::CompletionResponse::Array(vec![
                        lsp::CompletionItem {
                            label: "target".into(),
                            text_edit: Some(lsp::CompletionTextEdit::Edit(lsp::TextEdit {
                                range: lsp::Range::new(
                                    lsp::Position::new(0, 12),
                                    lsp::Position::new(0, 15),
                                ),
                                new_text: "target".into(),
                            })),
                            ..Default::default()
                        },
                    ])))
                }
            }
        });
        let mut definitions = server.set_request_handler::<lsp::request::GotoDefinition, _, _>({
            let source_uri = source_uri.clone();
            let target_uri = lsp::Uri::from_file_path(&target_path).expect("definition URI");
            move |params, _| {
                assert_eq!(
                    params.text_document_position_params.text_document.uri,
                    source_uri
                );
                assert_eq!(
                    params.text_document_position_params.position,
                    lsp::Position::new(0, 14)
                );
                let target_uri = target_uri.clone();
                async move {
                    Ok(Some(lsp::GotoDefinitionResponse::Scalar(lsp::Location {
                        uri: target_uri,
                        range: lsp::Range::new(lsp::Position::new(0, 7), lsp::Position::new(0, 13)),
                    })))
                }
            }
        });

        for (style, selector) in [
            (DiffViewStyle::Unified, "ICON-DiffUnified"),
            (DiffViewStyle::Split, "ICON-DiffSplit"),
        ] {
            workspace.update_in(cx, |workspace, window, cx| {
                assert!(workspace.activate_item(&view, true, true, window, cx));
            });
            cx.update(|window, cx| {
                window.refresh();
                window.draw(cx).clear(cx);
            });
            let button = cx.debug_bounds(selector).expect("native diff mode button");
            cx.simulate_click(button.center(), gpui::Modifiers::none());
            cx.run_until_parked();
            cx.update(|window, cx| {
                window.refresh();
                window.draw(cx).clear(cx);
            });
            view.read_with(cx, |view, cx| {
                assert_eq!(view.editor.read(cx).diff_view_style(), style);
                assert_eq!(
                    view.editor.read(cx).is_split(),
                    style == DiffViewStyle::Split
                );
                assert_eq!(view.buffer, buffer);
                assert!(!editor.read(cx).read_only(cx));
                if let Some(left) = view.editor.read(cx).lhs_editor() {
                    assert!(left.read(cx).read_only(cx));
                }
            });
            ordinary_editor.update_in(cx, |editor, window, cx| {
                editor.set_text("fn main() { tar }\n", window, cx)
            });
            cx.run_until_parked();
            editor.update_in(cx, |editor, window, cx| {
                let anchor = editor
                    .buffer()
                    .read(cx)
                    .buffer_point_to_anchor(&buffer, Point::new(0, 15), cx)
                    .expect("live completion position");
                editor.change_selections(
                    editor::SelectionEffects::no_scroll(),
                    window,
                    cx,
                    |selections| selections.select_ranges([anchor..anchor]),
                );
                window.focus(&editor.focus_handle(cx), cx);
            });
            cx.dispatch_action(editor::actions::ShowCompletions);
            completions
                .next()
                .await
                .expect("completion reached the language server");
            cx.condition(&editor, |editor, _| {
                editor
                    .current_completions()
                    .is_some_and(|items| items.iter().any(|item| item.new_text == "target"))
            })
            .await;
            editor
                .update_in(cx, |editor, window, cx| {
                    editor.confirm_completion(
                        &editor::actions::ConfirmCompletion::default(),
                        window,
                        cx,
                    )
                })
                .expect("completion edit")
                .await
                .expect("apply completion");
            assert_eq!(
                buffer.read_with(cx, |buffer, _| buffer.text()),
                "fn main() { target }\n"
            );
            editor.update_in(cx, |editor, window, cx| {
                let anchor = editor
                    .buffer()
                    .read(cx)
                    .buffer_point_to_anchor(&buffer, buffer.read(cx).max_point(), cx)
                    .expect("editable end of file");
                editor.change_selections(
                    editor::SelectionEffects::no_scroll(),
                    window,
                    cx,
                    |selections| selections.select_ranges([anchor..anchor]),
                );
                editor.insert("// edited in diff\n", window, cx);
            });
            let expected = "fn main() { target }\n// edited in diff\n";
            assert_eq!(
                ordinary_editor.read_with(cx, |editor, cx| editor.text(cx)),
                expected
            );
            view.update_in(cx, |view, window, cx| {
                view.save(SaveOptions::default(), project.clone(), window, cx)
            })
            .await
            .expect("save from diff");
            assert_eq!(
                fs.load(source_uri.to_file_path().expect("source path").as_path())
                    .await
                    .expect("saved source"),
                expected
            );
            assert!(!view.read_with(cx, |view, cx| view.is_dirty(cx)));

            let message = format!("diagnostic in {style:?}");
            server.notify::<lsp::notification::PublishDiagnostics>(lsp::PublishDiagnosticsParams {
                uri: source_uri.clone(),
                version: None,
                diagnostics: vec![lsp::Diagnostic {
                    range: lsp::Range::new(lsp::Position::new(0, 12), lsp::Position::new(0, 18)),
                    severity: Some(lsp::DiagnosticSeverity::ERROR),
                    message: lsp::DiagnosticMessage::String(message.clone()),
                    ..Default::default()
                }],
            });
            cx.condition(&buffer, |buffer, _| {
                buffer
                    .snapshot()
                    .diagnostics_in_range::<_, Point>(Point::zero()..buffer.max_point(), false)
                    .any(|entry| entry.diagnostic.message.as_str() == message)
            })
            .await;
            editor.read_with(cx, |editor, cx| {
                let multibuffer = editor.buffer().read(cx);
                let expected_start = multibuffer
                    .buffer_point_to_anchor(&buffer, Point::new(0, 12), cx)
                    .expect("diagnostic position");
                let snapshot = multibuffer.snapshot(cx);
                let diagnostic = snapshot
                    .diagnostics_in_range(Point::zero()..snapshot.max_point())
                    .find(|entry| entry.diagnostic.message.as_str() == message)
                    .expect("diagnostic reaches diff display");
                assert_eq!(diagnostic.range.start, expected_start.to_point(&snapshot));
            });
            editor.update_in(cx, |editor, window, cx| {
                let anchor = editor
                    .buffer()
                    .read(cx)
                    .buffer_point_to_anchor(&buffer, Point::new(0, 14), cx)
                    .expect("definition source");
                editor.change_selections(
                    editor::SelectionEffects::no_scroll(),
                    window,
                    cx,
                    |selections| selections.select_ranges([anchor..anchor]),
                );
                window.focus(&editor.focus_handle(cx), cx);
            });
            editor
                .update_in(cx, |editor, window, cx| {
                    editor.go_to_definition(&editor::actions::GoToDefinition::default(), window, cx)
                })
                .await
                .expect("navigate from diff");
            definitions
                .next()
                .await
                .expect("definition reached language server");
            workspace.read_with(cx, |workspace, cx| {
                let target = workspace
                    .active_pane()
                    .read(cx)
                    .active_item()
                    .and_then(|item| item.downcast::<Editor>())
                    .expect("native definition editor");
                assert_eq!(target.read(cx).text(cx), "pub fn target() {}\n");
            });
            workspace
                .update_in(cx, |workspace, window, cx| {
                    let pane = workspace.active_pane().downgrade();
                    workspace.go_back(pane, window, cx)
                })
                .await
                .expect("return from definition to diff");
            workspace.read_with(cx, |workspace, cx| {
                assert_eq!(
                    workspace.active_item(cx).map(|item| item.item_id()),
                    Some(view.entity_id())
                )
            });
        }
    }

    #[gpui::test(iterations = 3)]
    async fn kk_remote_diff_restores_session_without_opening_extra_tabs(
        cx: &mut TestAppContext,
        server_cx: &mut TestAppContext,
    ) {
        run_diff_session_restore(cx, server_cx)
            .await
            .expect("restore diff session");
    }

    async fn run_diff_session_restore(
        cx: &mut TestAppContext,
        server_cx: &mut TestAppContext,
    ) -> Result<()> {
        use crate::kk_panel::remote_tests::RemoteFixture;
        use workspace::ItemHandle as _;

        let fixture = RemoteFixture::create(cx, server_cx).await?;
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
        let revision = snapshot.exact_revision("@").await?;
        let original = std::fs::read(fixture.root.join("file.txt"))?;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        cx.update(|_, cx| init(cx));
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
                KkDiff::open(
                    snapshot.clone(),
                    DiffTarget::change(&revision),
                    ChangedFile {
                        path: "file.txt".into(),
                        change_type: ChangeKind::Modified,
                        renamed_from: None,
                        has_conflict: false,
                    },
                    true,
                    workspace.downgrade(),
                    DiffOpenMode::Open,
                    window,
                    cx,
                )
            })
            .await?;
        view.update(cx, |view, cx| {
            view.buffer.update(cx, |buffer, cx| {
                buffer.edit([(0..0, "unsaved prefix\n")], None, cx)
            });
        });
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            let editor = view.editor.read(cx).rhs_editor().clone();
            editor.update(cx, |editor, cx| {
                editor.change_selections(
                    editor::SelectionEffects::no_scroll(),
                    window,
                    cx,
                    |selections| {
                        selections.select_ranges([Point::new(1, 4)..Point::new(0, 2)]);
                    },
                )
            });
        });
        view.read_with(cx, |_, cx| view.to_serializable_item_handle(cx))
            .context("serializable diff")?;
        let item_id = u64::MAX;
        workspace
            .update(cx, |workspace, cx| {
                view.update(cx, |view, cx| view.serialize(workspace, item_id, false, cx))
            })
            .context("serialize")?
            .await?;
        let store = cx.update(|_, cx| KeyValueStore::global(cx));
        let namespace = session_namespace(workspace_id);
        let saved = store
            .scoped(&namespace)
            .read(&item_id.to_string())?
            .context("saved tab")?;
        assert!(saved.contains("unsaved prefix"));
        let saved_tab: SavedDiffTab = serde_json::from_str(&saved)?;
        assert_eq!(
            serde_json::to_value(saved_tab.editor)?["selections"],
            json!([[[1, 4], [0, 2]]])
        );
        let restored_workspace = cx.new_window_entity(|window, cx| {
            Workspace::new(Some(workspace_id), project.clone(), app_state, window, cx)
        });
        view.update_in(cx, |view, window, cx| {
            view.reload(project.clone(), window, cx)
        })
        .await?;
        let restored = cx
            .update(|window, cx| {
                KkDiff::deserialize(
                    project.clone(),
                    restored_workspace.downgrade(),
                    workspace_id,
                    item_id,
                    window,
                    cx,
                )
            })
            .await?;
        restored.read_with(cx, |restored, cx| {
            assert_eq!(restored.buffer, view.read(cx).buffer);
            assert_eq!(
                restored.buffer.read(cx).text(),
                "unsaved prefix\ncaptured\n"
            );
            assert!(restored.is_dirty(cx));
            assert_eq!(restored.base.as_deref(), Some("base\n"));
            assert_eq!(
                restored.snapshot.as_ref().expect("snapshot").operation(),
                snapshot.operation()
            );
            assert_eq!(restored_workspace.read(cx).items(cx).count(), 0);
        });
        cx.run_until_parked();
        restored.update(cx, |view, cx| {
            let editor = view.editor.read(cx).rhs_editor().clone();
            let saved = editor.update(cx, SerializedEditorState::capture);
            assert_eq!(
                serde_json::to_value(saved).expect("serialize editor state")["selections"],
                json!([[[1, 4], [0, 2]]])
            );
        });
        assert_eq!(std::fs::read(fixture.root.join("file.txt"))?, original);

        restored.update(cx, |view, cx| {
            view.buffer.update(cx, |buffer, cx| {
                buffer.edit([(0..0, "newer edit\n")], None, cx)
            })
        });
        let second = cx
            .update(|window, cx| {
                KkDiff::deserialize(
                    project.clone(),
                    restored_workspace.downgrade(),
                    workspace_id,
                    item_id,
                    window,
                    cx,
                )
            })
            .await?;
        assert!(
            second
                .read_with(cx, |view, cx| view.buffer.read(cx).text())
                .starts_with("newer edit\n")
        );
        let other_workspace_id = database.next_id().await?;
        assert!(
            cx.update(|window, cx| KkDiff::deserialize(
                project.clone(),
                restored_workspace.downgrade(),
                other_workspace_id,
                item_id,
                window,
                cx
            ))
            .await
            .is_err()
        );
        let mut wrong_host: SavedDiffTab = serde_json::from_str(&saved)?;
        wrong_host.host = Some("another-host".into());
        store
            .scoped(&namespace)
            .write(item_id.to_string(), serde_json::to_string(&wrong_host)?)
            .await?;
        assert!(
            cx.update(|window, cx| KkDiff::deserialize(
                project.clone(),
                restored_workspace.downgrade(),
                workspace_id,
                item_id,
                window,
                cx
            ))
            .await
            .is_err()
        );
        store
            .scoped(&namespace)
            .write(item_id.to_string(), saved)
            .await?;
        cx.update(|_, cx| cx.update_global::<SettingsStore, _>(|settings, cx| {
            settings.set_user_settings(r#"{"kk_workbench":{"auto_refresh":false},"session":{"restore_unsaved_buffers":false}}"#, cx).expect("saved only settings");
        }));
        view.update_in(cx, |view, window, cx| {
            view.reload(project.clone(), window, cx)
        })
        .await?;
        let clean = cx
            .update(|window, cx| {
                KkDiff::deserialize(
                    project.clone(),
                    restored_workspace.downgrade(),
                    workspace_id,
                    item_id,
                    window,
                    cx,
                )
            })
            .await?;
        clean.read_with(cx, |view, cx| {
            assert_eq!(view.buffer.read(cx).text(), "captured\n");
            assert!(!view.is_dirty(cx));
        });
        let historical = cx
            .update(|window, cx| {
                KkDiff::open(
                    snapshot,
                    DiffTarget::change(&revision),
                    ChangedFile {
                        path: "file.txt".into(),
                        change_type: ChangeKind::Modified,
                        renamed_from: None,
                        has_conflict: false,
                    },
                    false,
                    workspace.downgrade(),
                    DiffOpenMode::Open,
                    window,
                    cx,
                )
            })
            .await?;
        let historical_id = item_id - 1;
        workspace
            .update(cx, |workspace, cx| {
                historical.update(cx, |view, cx| {
                    view.serialize(workspace, historical_id, false, cx)
                })
            })
            .context("serialize historical diff")?
            .await?;
        let historical = cx
            .update(|window, cx| {
                KkDiff::deserialize(
                    project.clone(),
                    restored_workspace.downgrade(),
                    workspace_id,
                    historical_id,
                    window,
                    cx,
                )
            })
            .await?;
        historical.read_with(cx, |view, cx| {
            assert_eq!(view.buffer.read(cx).capability(), Capability::ReadOnly);
            assert_eq!(view.buffer.read(cx).text(), "captured\n");
            assert_eq!(view.base.as_deref(), Some("base\n"));
            assert!(!view.is_dirty(cx));
            assert!(view.buffer.read(cx).file().is_some());
            assert!(!view.has_working_copy_buffer(cx));
            assert!(matches!(
                view.blame_targets(cx).first().map(|(_, _, target)| target),
                Some(super::super::kk_blame::BlameTarget::Revision { .. })
            ));
        });
        cx.update(|window, cx| KkDiff::cleanup(workspace_id, vec![], window, cx))
            .await?;
        assert!(
            store
                .scoped(&namespace)
                .read(&item_id.to_string())?
                .is_none()
        );
        Ok(())
    }

    #[gpui::test(iterations = 3)]
    async fn refresh_preserves_live_draft_selection_and_focus(cx: &mut TestAppContext) {
        cx.background_executor.allow_parking();
        let (directory, initial, latest, revision) = cx
            .background_spawn(async move {
                futures_lite::future::block_on(async {
                    let directory = tempfile::tempdir()?;
                    let root = directory.path();
                    run_kk(root, &["init", "."]).await?;
                    run_kk(
                        root,
                        &["config", "set", "--repo", "user.name", "KK Workbench Test"],
                    )
                    .await?;
                    run_kk(
                        root,
                        &[
                            "config",
                            "set",
                            "--repo",
                            "user.email",
                            "test@example.invalid",
                        ],
                    )
                    .await?;
                    std::fs::write(root.join("file.txt"), "base\n")?;
                    run_kk(root, &["new", "-m", "child"]).await?;
                    std::fs::write(root.join("file.txt"), "saved\n")?;
                    std::fs::write(root.join("binary.dat"), b"\0\xffbinary")?;
                    let repository = kk_workbench::Repository::open(root, Path::new("kk")).await?;
                    let initial = repository.snapshot(true).await?;
                    let revision = initial
                        .revisions(Some("@"), 1)
                        .await?
                        .pop()
                        .context("working copy")?;
                    run_kk(root, &["edit", "@-"]).await?;
                    std::fs::write(root.join("file.txt"), "updated base\n")?;
                    run_kk(root, &["edit", &revision.full_change_id]).await?;
                    let latest = repository.snapshot(true).await?;
                    anyhow::Ok((directory, initial, latest, revision))
                })
            })
            .await
            .expect("prepare refresh snapshots");
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(initial.repository().root(), json!({"file.txt": "saved\n"}))
            .await;
        let project = Project::test(fs, [initial.repository().root()], cx).await;
        let buffer = project
            .update(cx, |project, cx| {
                project.open_local_buffer(initial.repository().root().join("file.txt"), cx)
            })
            .await
            .expect("shared buffer");
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |workspace, _| workspace.workspace().clone());
        let diff = cx.new(|cx| BufferDiff::new(&buffer.read(cx).snapshot().text, None, None, cx));
        let view = cx.new_window_entity(|window, cx| {
            KkDiff::new(
                DiffIdentity {
                    root: latest.repository().root().to_owned(),
                    scope: revision.revset(),
                    path: "file.txt".into(),
                    working_copy: true,
                    presentation: DiffPresentation::Text,
                },
                DiffTarget::change(&revision),
                ChangedFile {
                    path: "file.txt".into(),
                    change_type: ChangeKind::Modified,
                    renamed_from: None,
                    has_conflict: false,
                },
                buffer.clone(),
                diff.clone(),
                Some("base\n".into()),
                project,
                workspace.clone(),
                window,
                cx,
            )
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(Box::new(view.clone()), None, true, window, cx)
        });
        let editor = view.read_with(cx, |view, cx| view.editor.read(cx).rhs_editor().clone());
        assert_eq!(
            view.update_in(cx, |view, window, cx| view.search_query(window, cx)),
            Some("saved".into())
        );
        editor.update_in(cx, |editor, window, cx| {
            editor.change_selections(
                editor::SelectionEffects::no_scroll(),
                window,
                cx,
                |selections| selections.select_ranges([Point::new(1, 0)..Point::new(1, 0)]),
            );
        });
        assert_eq!(
            view.update_in(cx, |view, window, cx| view.search_query(window, cx)),
            None
        );
        view.update(cx, |view, cx| view.refresh(initial.clone(), cx));
        let reopened = cx.update(|window, cx| {
            KkDiff::open(
                latest.clone(),
                DiffTarget::change(&revision),
                view.read(cx).file.clone(),
                true,
                workspace.downgrade(),
                DiffOpenMode::Open,
                window,
                cx,
            )
            .now_or_never()
            .expect("existing diff must open without waiting for repository reads")
            .expect("reuse existing diff")
        });
        assert_eq!(reopened, view);
        view.update(cx, |view, cx| {
            let generation = view.refresh_generation;
            view.refresh(latest.clone(), cx);
            assert_eq!(
                view.refresh_generation, generation,
                "join the pending refresh"
            );
        });
        let task = view.update(cx, |view, _| {
            std::mem::replace(&mut view.refresh_task, Task::ready(()))
        });
        buffer.update(cx, |buffer, cx| {
            buffer.edit([(0..0, "new draft\n")], None, cx);
        });
        editor.update_in(cx, |editor, window, cx| {
            editor.change_selections(
                editor::SelectionEffects::no_scroll(),
                window,
                cx,
                |selections| selections.select_ranges([Point::new(1, 1)..Point::new(1, 3)]),
            );
        });
        let other = cx.new_window_entity(Editor::single_line);
        let focus = other.read_with(cx, |editor, cx| editor.focus_handle(cx));
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(Box::new(other), None, true, window, cx);
        });
        task.await;
        cx.run_until_parked();
        assert_eq!(
            buffer.read_with(cx, |buffer, _| buffer.text()),
            "new draft\nsaved\n"
        );
        assert!(buffer.read_with(cx, |buffer, _| buffer.is_dirty()));
        assert_eq!(
            diff.read_with(cx, |diff, cx| diff.base_text_buffer().read(cx).text()),
            "updated base\n"
        );
        assert_eq!(cx.update(|window, cx| window.focused(cx)), Some(focus));
        editor.update(cx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            assert_eq!(
                editor.selections.newest_adjusted(&snapshot).range(),
                Point::new(1, 1)..Point::new(1, 3)
            );
        });
        assert!(view.read_with(cx, |view, _| view.error.is_none()));
        let lifetime = kk_workbench::RepositoryLifetime::default();
        let closed = initial.with_lifetime(Some(lifetime.clone()));
        let stale_refresh = view.update(cx, |view, cx| {
            view.refresh(closed, cx);
            std::mem::replace(&mut view.refresh_task, Task::ready(()))
        });
        lifetime.invalidate();
        stale_refresh.await;
        view.read_with(cx, |view, cx| {
            assert_eq!(
                view.snapshot
                    .as_ref()
                    .expect("current snapshot")
                    .operation(),
                latest.operation()
            );
            assert_eq!(
                view.diff.read(cx).base_text_buffer().read(cx).text(),
                "updated base\n"
            );
            assert!(
                view.error.is_none(),
                "closed repository results must be ignored"
            );
        });
        let target = view.read_with(cx, |view, _| view.target.clone());
        let information = cx
            .update(|window, cx| {
                KkDiff::open(
                    latest.clone(),
                    target.clone(),
                    ChangedFile {
                        path: "binary.dat".into(),
                        change_type: ChangeKind::Added,
                        renamed_from: None,
                        has_conflict: false,
                    },
                    true,
                    workspace.downgrade(),
                    DiffOpenMode::Open,
                    window,
                    cx,
                )
            })
            .await
            .expect("open binary file information");
        information.read_with(cx, |view, cx| {
            assert!(view.identity.presentation == DiffPresentation::FileInformation);
            assert_eq!(view.capability(cx), Capability::ReadOnly);
            assert!(!view.can_save(cx));
            assert!(view.buffer.read(cx).text().contains("Size: 8 bytes"));
            assert!(view.buffer.read(cx).text().contains("SHA-256:"));
        });
        let historical = cx
            .update(|window, cx| {
                KkDiff::open(
                    latest,
                    target,
                    ChangedFile {
                        path: "binary.dat".into(),
                        change_type: ChangeKind::Added,
                        renamed_from: None,
                        has_conflict: false,
                    },
                    false,
                    workspace.downgrade(),
                    DiffOpenMode::Preview,
                    window,
                    cx,
                )
            })
            .await
            .expect("historical information preview");
        cx.run_until_parked();
        let before = historical.read_with(cx, |view, cx| view.buffer.read(cx).text());
        let refresh = historical.update(cx, |view, cx| {
            view.refresh(initial, cx);
            std::mem::replace(&mut view.refresh_task, Task::ready(()))
        });
        refresh.await;
        cx.run_until_parked();
        assert_ne!(
            historical.read_with(cx, |view, cx| view.buffer.read(cx).text()),
            before
        );
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(
                workspace.active_pane().read(cx).preview_item_id(),
                Some(historical.entity_id()),
                "programmatic historical refresh must retain preview status"
            );
        });
        assert_eq!(
            std::fs::read(directory.path().join("binary.dat")).expect("binary remains unchanged"),
            b"\0\xffbinary"
        );
        drop(directory);
    }

    #[gpui::test]
    async fn working_copy_diff_preserves_shared_drafts_and_saves(cx: &mut TestAppContext) {
        cx.background_executor.allow_parking();
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.editor.diff_view_style = Some(DiffViewStyle::Split);
                });
            });
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/project", json!({"file.txt": "saved\n"}))
            .await;
        let project = Project::test(fs.clone(), [PathBuf::from("/project").as_path()], cx).await;
        let buffer = project
            .update(cx, |project, cx| {
                project.open_local_buffer("/project/file.txt", cx)
            })
            .await
            .expect("open native project buffer");
        buffer.update(cx, |buffer, cx| {
            buffer.edit([(0..0, "draft\n")], None, cx);
        });
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |workspace, _| workspace.workspace().clone());
        let base = Some(Arc::<str>::from("base\n"));
        let diff = cx.new(|cx| BufferDiff::new(&buffer.read(cx).snapshot().text, None, None, cx));
        diff.update(cx, |diff, cx| {
            diff.set_base_text(base.clone(), buffer.read(cx).snapshot().text, cx)
        })
        .await;
        let view = cx.new_window_entity(|window, cx| {
            KkDiff::new(
                DiffIdentity {
                    root: "/project".into(),
                    scope: "change".into(),
                    path: "file.txt".into(),
                    working_copy: true,
                    presentation: DiffPresentation::Text,
                },
                DiffTarget {
                    scope: DiffScope::Change("change".into()),
                    identity: "change".into(),
                    label: "change".into(),
                    reference: "kkkkkkkk".into(),
                    comparison: None,
                    revision: None,
                    category: None,
                },
                ChangedFile {
                    path: "file.txt".into(),
                    change_type: ChangeKind::Renamed,
                    renamed_from: Some("old file.txt".into()),
                    has_conflict: false,
                },
                buffer.clone(),
                diff.clone(),
                base,
                project.clone(),
                workspace.clone(),
                window,
                cx,
            )
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(Box::new(view.clone()), None, true, window, cx);
        });
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |view, _| view.buffer.clone()), buffer);
        assert!(view.read_with(cx, |view, cx| view.is_dirty(cx)));
        assert_eq!(
            buffer.read_with(cx, |buffer, _| buffer.text()),
            "draft\nsaved\n"
        );

        let same_buffer = project
            .update(cx, |project, cx| {
                project.open_local_buffer("/project/file.txt", cx)
            })
            .await
            .expect("reopen ordinary file");
        assert_eq!(same_buffer, buffer);
        buffer.update(cx, |buffer, cx| {
            buffer.edit([(0..0, "newer\n")], None, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |view, cx| view.buffer.read(cx).text()),
            "newer\ndraft\nsaved\n"
        );
        assert_eq!(
            diff.read_with(cx, |diff, cx| diff.base_text_buffer().read(cx).text()),
            "base\n"
        );

        view.update_in(cx, |view, window, cx| {
            view.save(SaveOptions::default(), project.clone(), window, cx)
        })
        .await
        .expect("save native diff buffer");
        assert!(!view.read_with(cx, |view, cx| view.is_dirty(cx)));
        assert_eq!(
            fs.load(Path::new("/project/file.txt"))
                .await
                .expect("read saved file"),
            "newer\ndraft\nsaved\n"
        );
        assert_eq!(
            buffer.read_with(cx, |buffer, _| buffer.text()),
            "newer\ndraft\nsaved\n"
        );
        view.update(cx, |view, cx| view.copy_reference(false, cx));
        assert_eq!(
            cx.update(|_, cx| cx.read_from_clipboard().and_then(|item| item.text())),
            Some("file.txt (kkkkkkkk)".into())
        );
        let editor = view.read_with(cx, |view, cx| view.editor.read(cx).rhs_editor().clone());
        editor.update_in(cx, |editor, window, cx| {
            window.focus(&editor.focus_handle(cx), cx);
            editor.change_selections(
                editor::SelectionEffects::no_scroll(),
                window,
                cx,
                |selections| {
                    selections.select_ranges([Point::new(0, 0)..Point::new(2, 0)]);
                },
            );
        });
        view.update(cx, |view, cx| view.copy_reference(true, cx));
        assert_eq!(
            cx.update(|_, cx| cx.read_from_clipboard().and_then(|item| item.text())),
            Some("file.txt:1~2 (kkkkkkkk)".into())
        );
        assert_eq!(
            view.update(cx, |view, cx| view.classification_ranges(cx))
                .expect("right selection"),
            vec![LineRange {
                side: SelectionSide::Modified,
                begin: 1,
                end: 2
            }]
        );
        let left = view
            .read_with(cx, |view, cx| view.editor.read(cx).lhs_editor().cloned())
            .expect("split diff has left editor");
        left.update_in(cx, |editor, window, cx| {
            window.focus(&editor.focus_handle(cx), cx);
            editor.change_selections(
                editor::SelectionEffects::no_scroll(),
                window,
                cx,
                |selections| {
                    selections.select_ranges([Point::new(0, 0)..Point::new(1, 0)]);
                },
            );
        });
        cx.run_until_parked();
        view.update(cx, |view, cx| view.copy_reference(true, cx));
        assert_eq!(
            cx.update(|_, cx| cx.read_from_clipboard().and_then(|item| item.text())),
            Some("old file.txt:1 (kkkkkkkk)".into())
        );
        assert_eq!(
            view.update(cx, |view, cx| view.classification_ranges(cx))
                .expect("left selection"),
            vec![LineRange {
                side: SelectionSide::Original,
                begin: 1,
                end: 1
            }]
        );
        left.update_in(cx, |editor, window, cx| {
            editor.change_selections(
                editor::SelectionEffects::no_scroll(),
                window,
                cx,
                |selections| {
                    selections.select_ranges([Point::new(1, 0)..Point::new(1, 0)]);
                },
            )
        });
        assert_eq!(
            view.update(cx, |view, cx| view.classification_ranges(cx))
                .expect("EOF cursor selects the final hunk"),
            vec![LineRange {
                side: SelectionSide::Original,
                begin: 1,
                end: 1
            }]
        );
        let removed = kk_workbench::parse_git_diff("diff --git a/old b/new\n--- a/old\n+++ b/new\n@@ -1 +1,3 @@\n-base\n+newer\n+draft\n+saved\n")
            .expect("parse navigation fixture").into_iter().find(|line| line.kind == LineKind::Removed).expect("removed search result");
        for style in [DiffViewStyle::Split, DiffViewStyle::Unified] {
            cx.update(|_, cx| {
                cx.update_global::<SettingsStore, _>(|store, cx| {
                    store.update_user_settings(cx, |settings| {
                        settings.editor.diff_view_style = Some(style)
                    });
                })
            });
            cx.run_until_parked();
            view.update_in(cx, |view, window, cx| {
                view.reveal_line(&removed, window, cx)
            });
            cx.run_until_parked();
            view.update(cx, |view, cx| view.copy_reference(true, cx));
            assert_eq!(
                cx.update(|_, cx| cx.read_from_clipboard().and_then(|item| item.text())),
                Some("old file.txt:1 (kkkkkkkk)".into()),
                "navigation style: {style:?}"
            );
            assert_eq!(
                view.update(cx, |view, cx| view.classification_ranges(cx))
                    .expect("cursor hunk"),
                vec![LineRange {
                    side: SelectionSide::Original,
                    begin: 1,
                    end: 1
                }],
                "hunk style: {style:?}"
            );
        }

        let first = view
            .update(cx, |view, cx| {
                view.preview_classification(
                    &[LineRange {
                        side: SelectionSide::Modified,
                        begin: 2,
                        end: 2,
                    }],
                    TransferDirection::Backward,
                    cx,
                )
            })
            .expect("first preview")
            .0;
        cx.run_until_parked();
        assert_eq!(
            diff.read_with(cx, |diff, cx| diff.base_text_buffer().read(cx).text()),
            "base\ndraft\n"
        );
        let second = view
            .update(cx, |view, cx| {
                view.preview_classification(
                    &[LineRange {
                        side: SelectionSide::Modified,
                        begin: 3,
                        end: 3,
                    }],
                    TransferDirection::Backward,
                    cx,
                )
            })
            .expect("second preview")
            .0;
        cx.run_until_parked();
        assert_eq!(
            diff.read_with(cx, |diff, cx| diff.base_text_buffer().read(cx).text()),
            "base\ndraft\nsaved\n"
        );
        assert_eq!(
            buffer.read_with(cx, |buffer, _| buffer.text()),
            "newer\ndraft\nsaved\n"
        );
        assert!(!buffer.read_with(cx, |buffer, _| buffer.is_dirty()));
        view.update(cx, |view, cx| {
            view.finish_classification_preview(
                first,
                Err(super::super::kk_classification::ClassificationFailure {
                    message: "first failed".into(),
                    uncertain: false,
                }),
                cx,
            )
        });
        cx.run_until_parked();
        assert_eq!(
            diff.read_with(cx, |diff, cx| diff.base_text_buffer().read(cx).text()),
            "base\nsaved\n"
        );
        buffer.update(cx, |buffer, cx| {
            buffer.edit([(0..0, "later draft\n")], None, cx)
        });
        view.update(cx, |view, cx| {
            view.finish_classification_preview(
                second,
                Err(super::super::kk_classification::ClassificationFailure {
                    message: "second failed".into(),
                    uncertain: false,
                }),
                cx,
            )
        });
        cx.run_until_parked();
        assert_eq!(
            diff.read_with(cx, |diff, cx| diff.base_text_buffer().read(cx).text()),
            "base\n"
        );
        assert_eq!(
            buffer.read_with(cx, |buffer, _| buffer.text()),
            "later draft\nnewer\ndraft\nsaved\n"
        );
        assert!(buffer.read_with(cx, |buffer, _| buffer.is_dirty()));
        assert!(view.read_with(cx, |view, _| view.classification.is_none()));
        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
        let closing = pane.update_in(cx, |pane, window, cx| {
            pane.close_item_by_id(view.entity_id(), workspace::SaveIntent::Close, window, cx)
        });
        cx.run_until_parked();
        assert!(cx.has_pending_prompt());
        cx.simulate_prompt_answer("Don't Save");
        closing.await.expect("discard unsaved diff edits");
        cx.run_until_parked();
        assert!(!buffer.read_with(cx, |buffer, _| buffer.is_dirty()));
        assert_eq!(
            buffer.read_with(cx, |buffer, _| buffer.text()),
            "newer\ndraft\nsaved\n"
        );

        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(Box::new(view.clone()), None, true, window, cx);
        });
        buffer.update(cx, |buffer, cx| {
            buffer.edit([(0..0, "local edit\n")], None, cx)
        });
        fs.insert_tree("/project", json!({"file.txt": "external edit\n"}))
            .await;
        cx.run_until_parked();
        assert!(view.read_with(cx, |view, cx| view.has_conflict(cx)));
        let closing = pane.update_in(cx, |pane, window, cx| {
            pane.close_item_by_id(view.entity_id(), workspace::SaveIntent::Close, window, cx)
        });
        cx.run_until_parked();
        cx.simulate_prompt_answer("Discard Edits");
        closing.await.expect("retain external file changes");
        assert_eq!(
            buffer.read_with(cx, |buffer, _| buffer.text()),
            "external edit\n"
        );
        assert_eq!(
            fs.load(Path::new("/project/file.txt"))
                .await
                .expect("external file"),
            "external edit\n"
        );
        let historical = cx.new(|cx| {
            let mut buffer = Buffer::local("A\nB\nC\n", cx);
            buffer.file_updated(
                Arc::new(crate::commit_view::GitBlob {
                    path: git::repository::RepoPath::new("category.txt").expect("historical path"),
                    worktree_id: project::WorktreeId::from_usize(0),
                    is_deleted: false,
                    is_binary: false,
                    display_name: "category.txt".into(),
                }),
                cx,
            );
            buffer.set_capability(Capability::ReadOnly, cx);
            buffer
        });
        let base = Some(Arc::<str>::from("a\nb\n"));
        let historical_diff =
            cx.new(|cx| BufferDiff::new(&historical.read(cx).snapshot().text, None, None, cx));
        historical_diff
            .update(cx, |diff, cx| {
                diff.set_base_text(base.clone(), historical.read(cx).snapshot().text, cx)
            })
            .await;
        let historical_view = cx.new_window_entity(|window, cx| {
            KkDiff::new(
                DiffIdentity {
                    root: "/project".into(),
                    scope: "category".into(),
                    path: "category.txt".into(),
                    working_copy: false,
                    presentation: DiffPresentation::Text,
                },
                DiffTarget {
                    scope: DiffScope::Change("category".into()),
                    identity: "category".into(),
                    label: "Reviewed".into(),
                    reference: "category".into(),
                    comparison: None,
                    revision: None,
                    category: Some(Category::Reviewed),
                },
                ChangedFile {
                    path: "category.txt".into(),
                    change_type: ChangeKind::Modified,
                    renamed_from: None,
                    has_conflict: false,
                },
                historical.clone(),
                historical_diff.clone(),
                base,
                project.clone(),
                workspace.clone(),
                window,
                cx,
            )
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(
                Box::new(historical_view.clone()),
                None,
                true,
                window,
                cx,
            )
        });
        let forward = historical_view
            .update(cx, |view, cx| {
                view.preview_classification(
                    &[LineRange {
                        side: SelectionSide::Modified,
                        begin: 1,
                        end: 1,
                    }],
                    TransferDirection::Forward,
                    cx,
                )
            })
            .expect("forward preview")
            .0;
        cx.run_until_parked();
        assert_eq!(
            historical.read_with(cx, |buffer, _| buffer.text()),
            "a\nB\nC\n"
        );
        let backward = historical_view
            .update(cx, |view, cx| {
                view.preview_classification(
                    &[LineRange {
                        side: SelectionSide::Modified,
                        begin: 2,
                        end: 2,
                    }],
                    TransferDirection::Backward,
                    cx,
                )
            })
            .expect("mixed direction preview")
            .0;
        cx.run_until_parked();
        assert_eq!(
            historical_diff.read_with(cx, |diff, cx| diff.base_text_buffer().read(cx).text()),
            "a\nB\n"
        );
        for id in [forward, backward] {
            historical_view.update(cx, |view, cx| {
                view.finish_classification_preview(
                    id,
                    Err(super::super::kk_classification::ClassificationFailure {
                        message: "rollback".into(),
                        uncertain: false,
                    }),
                    cx,
                )
            });
            cx.run_until_parked();
        }

        assert_eq!(
            historical.read_with(cx, |buffer, _| buffer.text()),
            "A\nB\nC\n"
        );
        assert!(!historical.read_with(cx, |buffer, _| buffer.is_dirty()));
        assert_eq!(
            historical_diff.read_with(cx, |diff, cx| diff.base_text_buffer().read(cx).text()),
            "a\nb\n"
        );
        let pending = historical_view
            .update(cx, |view, cx| {
                view.preview_classification(
                    &[LineRange {
                        side: SelectionSide::Modified,
                        begin: 3,
                        end: 3,
                    }],
                    TransferDirection::Forward,
                    cx,
                )
            })
            .expect("pending preview")
            .0;
        let following = historical_view
            .update(cx, |view, cx| {
                view.preview_classification(
                    &[LineRange {
                        side: SelectionSide::Modified,
                        begin: 1,
                        end: 1,
                    }],
                    TransferDirection::Forward,
                    cx,
                )
            })
            .expect("following preview")
            .0;
        cx.run_until_parked();
        historical_view.update(cx, |view, cx| {
            view.finish_classification_preview(
                pending,
                Err(super::super::kk_classification::ClassificationFailure {
                    message: "unknown publication".into(),
                    uncertain: true,
                }),
                cx,
            )
        });
        cx.run_until_parked();
        assert_eq!(
            historical.read_with(cx, |buffer, _| buffer.text()),
            "a\nB\n"
        );
        assert!(
            historical_view
                .update(cx, |view, cx| view.preview_classification(
                    &[LineRange {
                        side: SelectionSide::Modified,
                        begin: 1,
                        end: 1
                    }],
                    TransferDirection::Forward,
                    cx
                ))
                .is_err()
        );
        let (_directory, snapshot) = cx
            .background_spawn(async {
                let directory = tempfile::tempdir()?;
                run_kk(directory.path(), &["init", "."]).await?;
                let repository =
                    kk_workbench::Repository::open(directory.path(), Path::new("kk")).await?;
                anyhow::Ok((directory, repository.snapshot(false).await?))
            })
            .await
            .expect("reconciliation snapshot");
        historical_view.update(cx, |view, cx| {
            view.identity.root = snapshot.repository().root().to_owned();
            view.resolve_classification_uncertainty(&snapshot, false, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            historical.read_with(cx, |buffer, _| buffer.text()),
            "a\nB\nC\n"
        );
        assert!(historical_view.read_with(cx, |view, _| {
            view.classification
                .as_ref()
                .is_some_and(|preview| !preview.uncertain && preview.waiting.contains(&following))
        }));
        historical_view.update(cx, |view, cx| {
            view.finish_classification_preview(
                following,
                Err(super::super::kk_classification::ClassificationFailure {
                    message: "following request rejected".into(),
                    uncertain: false,
                }),
                cx,
            )
        });
        cx.run_until_parked();
        assert_eq!(
            historical.read_with(cx, |buffer, _| buffer.text()),
            "A\nB\nC\n"
        );
    }
}
