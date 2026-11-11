use super::{kk_classification::ConflictRequest, kk_panel::KkPanel};
use anyhow::{Context as _, Result, ensure};
use buffer_diff::BufferDiff;
use db::kvp::KeyValueStore;
use editor::items::SerializedEditorState;
use editor::{
    Addon, Editor, EditorEvent, EditorMode, RowHighlightOptions,
    display_map::{BlockPlacement, BlockProperties, BlockStyle, CustomBlockId},
    scroll::Autoscroll,
};
use futures::{FutureExt as _, future::Shared};
use gpui::{
    App, AppContext as _, AsyncWindowContext, Context, DismissEvent, Entity, EventEmitter,
    FocusHandle, Focusable, Render, SharedString, Subscription, Task, WeakEntity, Window, actions,
};
use kk_workbench::{
    ConflictChoice, ConflictDraft, ConflictEditReason, ConflictInput, ConflictRole, FileConflict,
    Snapshot,
};
use language::{Buffer, Capability};
use multi_buffer::{MultiBuffer, MultiBufferOffset, ToOffset as _};
use picker::{Picker, PickerDelegate};
use project::{
    DirectoryLister, Project,
    lsp_store::{FormatTrigger, LspFormatTarget},
};
use serde::{Deserialize, Serialize};
use settings::Settings;
use std::{any::TypeId, cell::RefCell, collections::HashSet, rc::Rc, sync::Arc};
use ui::{Button, ListItem, prelude::*};
use util::ResultExt as _;
use workspace::{
    Item, ItemId, ModalView, SerializableItem, Workspace, WorkspaceId,
    item::{ItemBufferKind, ItemEvent, SaveOptions},
    searchable::SearchableItemHandle,
};

actions!(
    kk_workbench,
    [
        NextConflict,
        PreviousConflict,
        AcceptAllConflictVersion,
        ApplyConflictResolution,
        DeleteConflictedFile,
        ExportConflictDraft,
        CompareConflictVersions
    ]
);

pub(super) struct KkConflict {
    draft: ConflictDraft,
    saved_draft: ConflictDraft,
    saved_version: u64,
    snapshot: Option<Snapshot>,
    operation: String,
    editor: Entity<Editor>,
    buffer: Entity<Buffer>,
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    panel: Option<WeakEntity<KkPanel>>,
    key: String,
    writes: Shared<Task<Result<(), String>>>,
    blocks: HashSet<CustomBlockId>,
    line_numbers: Rc<RefCell<Option<Arc<[Option<u32>]>>>>,
    undone: HashSet<clock::Lamport>,
    pending_reason: ConflictEditReason,
    update_scheduled: bool,
    applying: bool,
    error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

#[derive(Deserialize, Serialize)]
struct SavedConflictTab {
    key: String,
    draft: ConflictDraft,
    saved_draft: ConflictDraft,
    saved_version: u64,
    operation: String,
    host: Option<String>,
    #[serde(flatten)]
    editor: SerializedEditorState,
}

#[derive(Deserialize, Serialize)]
struct StoredConflictDraft {
    #[serde(flatten)]
    draft: ConflictDraft,
    #[serde(default)]
    saved_version: u64,
}

pub(super) fn init(cx: &mut App) {
    workspace::register_serializable_item::<KkConflict>(cx);
}

fn session_namespace(workspace_id: WorkspaceId) -> String {
    format!("KKConflictTabs:{}", i64::from(workspace_id))
}

struct ConflictAddon(Rc<RefCell<Option<Arc<[Option<u32>]>>>>);
struct BaseHighlight;
struct FirstSideHighlight;
struct OtherSideHighlight;

impl Addon for ConflictAddon {
    fn to_any(&self) -> &dyn std::any::Any {
        self
    }
    fn line_number_overrides(&self, _: &App) -> Option<Arc<[Option<u32>]>> {
        self.0.borrow().clone()
    }
}

impl KkConflict {
    pub(super) fn open(
        snapshot: Snapshot,
        revision: String,
        path: String,
        panel: WeakEntity<KkPanel>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<()>> {
        let Some(workspace) = workspace.upgrade() else {
            return Task::ready(Err(anyhow::anyhow!("Workspace was closed")));
        };
        let project = workspace.read(cx).project().clone();
        let workspace_id = workspace.read(cx).database_id();
        let transient_id = workspace_id
            .is_none()
            .then_some(workspace.entity_id().as_u64());
        let host = project
            .read(cx)
            .remote_connection_options(cx)
            .map(|options| options.host_key());
        let store = KeyValueStore::global(cx);
        window.spawn(cx, async move |cx| {
            let opening = snapshot.repository().begin_diff_opening().await?;
            let repository = snapshot.repository().clone();
            let source = snapshot.clone();
            let conflict = cx
                .background_spawn(async move { source.file_conflict(&revision, &path).await })
                .await?;
            repository.check_lifetime()?;
            // A queued write may be yielding to this read; release it before user input or writes.
            drop(opening);
            if conflict.binary {
                ensure!(
                    !conflict.immutable,
                    "This change is immutable. Create a mutable change to resolve its conflict."
                );
                let selection = workspace.update_in(cx, |workspace, window, cx| {
                    VersionPicker::choose(conflict.inputs.clone(), workspace, window, cx)
                })?;
                // A cancelled window must release the modal and its pending selection.
                drop(workspace);
                let Some(index) = selection.await else {
                    return Ok(());
                };
                let input = conflict.inputs.get(index).ok_or_else(|| {
                    anyhow::anyhow!("The selected conflict version is no longer available")
                })?;
                let request = ConflictRequest {
                    snapshot,
                    result: input.id.as_ref().map(|_| input.bytes.clone()),
                    executable: Some(input.executable),
                    conflict,
                };
                panel
                    .update(cx, |panel, cx| panel.resolve_conflict(request, cx))?
                    .await?;
                return Ok(());
            }

            let existing = workspace.read_with(cx, |workspace, cx| {
                workspace.items_of_type::<Self>(cx).find(|view| {
                    let view = view.read(cx);
                    view.draft.applied_digest.is_none()
                        && view.draft.conflict.repository_root == conflict.repository_root
                        && view.draft.conflict.commit_id == conflict.commit_id
                        && view.draft.conflict.path == conflict.path
                })
            });
            if let Some(existing) = existing {
                existing
                    .update(cx, |view, cx| {
                        view.operation = snapshot.operation().to_owned();
                        view.snapshot = Some(snapshot);
                        Self::refresh_metadata(&mut view.draft, &conflict);
                        view.persist(false, cx)
                    })
                    .await?;
                repository.check_lifetime()?;
                workspace.update_in(cx, |workspace, window, cx| {
                    workspace.activate_item(&existing, true, true, window, cx);
                    existing.update(cx, |view, cx| view.apply_completed(window, cx));
                })?;
                return Ok(());
            }

            let identity = (
                &conflict.repository_root,
                &conflict.commit_id,
                &conflict.path,
            );
            let index = format!(
                "KKConflictIndexV2:{}",
                serde_json::to_string(&(workspace_id, transient_id, &host, identity))?
            );
            let legacy_index = match host {
                Some(host) => format!(
                    "KKRemoteConflictIndex:{}",
                    serde_json::to_string(&(host, identity))?
                ),
                None => format!("KKConflictIndex:{}", serde_json::to_string(&identity)?),
            };
            let restored = cx
                .background_spawn(async move {
                    let scoped_key = store.read_kvp(&index)?;
                    let legacy = scoped_key.is_none();
                    let key = if legacy {
                        store.read_kvp(&legacy_index)?
                    } else {
                        scoped_key
                    };
                    let saved = key
                        .and_then(|key| {
                            store
                                .read_kvp(&key)
                                .transpose()
                                .map(|result| result.map(|value| (key, value)))
                        })
                        .transpose()?;
                    let (key, mut stored) = match saved {
                        Some((key, value)) => {
                            let mut stored: StoredConflictDraft = serde_json::from_str(&value)?;
                            if stored.draft.applied_digest.is_none() {
                                stored.draft.revert_to_saved();
                                let key = if legacy {
                                    format!("KKConflictDraft:{}", uuid::Uuid::new_v4())
                                } else {
                                    key
                                };
                                (key, stored)
                            } else {
                                (
                                    format!("KKConflictDraft:{}", uuid::Uuid::new_v4()),
                                    StoredConflictDraft {
                                        draft: ConflictDraft::open(conflict.clone())?,
                                        saved_version: 0,
                                    },
                                )
                            }
                        }
                        None => (
                            format!("KKConflictDraft:{}", uuid::Uuid::new_v4()),
                            StoredConflictDraft {
                                draft: ConflictDraft::open(conflict.clone())?,
                                saved_version: 0,
                            },
                        ),
                    };
                    Self::refresh_metadata(&mut stored.draft, &conflict);
                    store
                        .write_kvp(key.clone(), serde_json::to_string(&stored)?)
                        .await?;
                    store.write_kvp(index, key.clone()).await?;
                    anyhow::Ok((key, stored))
                })
                .await?;
            let (key, stored) = restored;
            let buffer = Self::draft_buffer(&stored.draft, &project, cx).await?;
            repository.check_lifetime()?;
            workspace.update_in(cx, |workspace, window, cx| {
                let workspace_entity = cx.weak_entity();
                let view = cx.new(|cx| {
                    let mut view = Self::new(
                        Some(snapshot),
                        stored.draft,
                        key,
                        buffer,
                        project,
                        workspace_entity,
                        Some(panel),
                        window,
                        cx,
                    );
                    view.saved_version = stored.saved_version;
                    view
                });
                workspace.add_item_to_active_pane(Box::new(view.clone()), None, true, window, cx);
                view.update(cx, |view, cx| {
                    view.refresh_blocks(window, cx);
                    view.navigate(true, window, cx);
                    view.apply_completed(window, cx);
                });
            })?;
            Ok(())
        })
    }

    fn new(
        snapshot: Option<Snapshot>,
        draft: ConflictDraft,
        key: String,
        buffer: Entity<Buffer>,
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        panel: Option<WeakEntity<KkPanel>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let multibuffer = cx.new(|cx| MultiBuffer::singleton(buffer.clone(), cx));
        let editor = cx.new(|cx| {
            Editor::new(
                EditorMode::full(),
                multibuffer,
                Some(project.clone()),
                window,
                cx,
            )
        });
        let line_numbers = Rc::new(RefCell::new(
            (!draft.conflicts.is_empty()).then(|| draft.line_numbers().into()),
        ));
        editor.update(cx, |editor, _| {
            editor.register_addon(ConflictAddon(line_numbers.clone()))
        });
        let subscription =
            cx.subscribe_in(&editor, window, |this: &mut Self, _, event, window, cx| {
                match event {
                    EditorEvent::TransactionUndone { transaction_id } => {
                        this.undone.insert(*transaction_id);
                        this.pending_reason = ConflictEditReason::Undo;
                    }
                    EditorEvent::Edited { transaction_id }
                        if this.undone.remove(transaction_id) =>
                    {
                        this.pending_reason = ConflictEditReason::Redo;
                    }
                    _ => {}
                }

                if matches!(
                    event,
                    EditorEvent::BufferEdited
                        | EditorEvent::Edited { .. }
                        | EditorEvent::TransactionUndone { .. }
                ) && !this.update_scheduled
                {
                    this.update_scheduled = true;
                    cx.defer_in(window, |this, window, cx| {
                        this.update_scheduled = false;
                        let reason =
                            std::mem::replace(&mut this.pending_reason, ConflictEditReason::Edit);
                        if let Err(error) = this.synchronize(reason, cx) {
                            this.error = Some(format!("{error:#}"));
                        }

                        this.refresh_blocks(window, cx);
                        cx.emit(EditorEvent::DirtyChanged);
                        cx.notify();
                    });
                }

                cx.emit(event.clone());
            });
        Self {
            saved_draft: draft.clone(),
            saved_version: 0,
            draft,
            operation: snapshot
                .as_ref()
                .map(|snapshot| snapshot.operation().to_owned())
                .unwrap_or_default(),
            snapshot,
            editor,
            buffer,
            project,
            workspace,
            panel,
            key,
            writes: Task::ready(Ok(())).shared(),
            blocks: HashSet::new(),
            line_numbers,
            undone: HashSet::new(),
            pending_reason: ConflictEditReason::Edit,
            update_scheduled: false,
            applying: false,
            error: None,
            _subscriptions: vec![subscription],
        }
    }

    async fn draft_buffer(
        draft: &ConflictDraft,
        project: &Entity<Project>,
        cx: &mut AsyncWindowContext,
    ) -> Result<Entity<Buffer>> {
        let registry = project.read_with(cx, |project, _| project.languages().clone());
        let language = match registry
            .language_for_file_path(&draft.conflict.repository_root.join(&draft.conflict.path))
        {
            Some(language) => Some(registry.load_language(language).await??),
            None => None,
        };
        let buffer = project
            .update(cx, |project, cx| project.create_buffer(language, false, cx))
            .await?;
        buffer.update(cx, |buffer, cx| {
            buffer.set_language_registry(registry);
            buffer.set_text(kk_workbench::normalize_conflict_text(&draft.saved_text), cx);
            if let Some(entry) = buffer.peek_undo_stack() {
                buffer.forget_transaction(entry.transaction_id());
            }

            buffer.did_save(buffer.version(), None, cx);
            if draft.text != draft.saved_text {
                buffer.set_text(draft.editor_text(), cx);
            }
        });
        Ok(buffer)
    }

    fn saved_snapshot(&self) -> ConflictDraft {
        let mut saved = self.saved_draft.clone();
        saved.conflict = self.draft.conflict.clone();
        saved.applied_digest = self.draft.applied_digest.clone();
        saved.applied_deleted = self.draft.applied_deleted;
        saved
    }

    fn synchronize(&mut self, reason: ConflictEditReason, cx: &App) -> Result<()> {
        self.draft
            .update(&self.buffer.read(cx).text(), None, reason)
    }

    fn refresh_metadata(draft: &mut ConflictDraft, conflict: &FileConflict) {
        draft.conflict.immutable = conflict.immutable;
        for input in &mut draft.conflict.inputs {
            if let Some(source) = conflict
                .inputs
                .iter()
                .find(|candidate| candidate.role == input.role && candidate.index == input.index)
                .and_then(|candidate| candidate.source.as_ref())
                && input.source.as_ref().is_none_or(|previous| {
                    !source.details_incomplete
                        || (previous.details_incomplete
                            && (previous.description.is_empty() || !source.description.is_empty()))
                })
            {
                input.source = Some(source.clone());
            }
        }
    }

    fn persist(&mut self, save_text: bool, cx: &mut Context<Self>) -> Task<Result<()>> {
        let saved = save_text.then(|| {
            let mut saved = self.draft.clone();
            saved.mark_saved();
            saved
        });
        let key = self.key.clone();
        let store = KeyValueStore::global(cx);
        let previous = self.writes.clone();
        let version = self.buffer.read(cx).version();
        self.writes = cx
            .spawn(async move |this, cx| {
                // A failed earlier save must not prevent a corrected draft from being saved.
                if let Err(error) = previous.await {
                    log::warn!("Earlier KK conflict draft save failed: {error}");
                }

                let stored = this
                    .read_with(cx, |this, _| StoredConflictDraft {
                        draft: saved.clone().unwrap_or_else(|| this.saved_snapshot()),
                        saved_version: this
                            .saved_version
                            .saturating_add(u64::from(saved.is_some())),
                    })
                    .map_err(|error| error.to_string())?;
                let saved_version = stored.saved_version;
                let result = cx
                    .background_spawn(async move {
                        store.write_kvp(key, serde_json::to_string(&stored)?).await
                    })
                    .await;
                if let Err(error) = result {
                    return Err(format!("Could not save conflict draft: {error:#}"));
                }

                this.update(cx, |this, cx| {
                    if let Some(saved) = &saved {
                        this.saved_version = saved_version;
                        this.saved_draft = saved.clone();
                        this.draft.acknowledge_save(saved);
                        this.buffer
                            .update(cx, |buffer, cx| buffer.did_save(version, None, cx));
                        cx.emit(EditorEvent::Saved);
                    }

                    cx.emit(EditorEvent::DirtyChanged);
                    cx.notify();
                })
                .map_err(|error| error.to_string())
            })
            .shared();
        let write = self.writes.clone();
        cx.spawn(async move |_, _| write.await).detach();
        let write = self.writes.clone();
        cx.spawn(async move |_, _| write.await.map_err(anyhow::Error::msg))
    }

    fn save_draft(
        &mut self,
        options: SaveOptions,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let formatting = options.format.then(|| {
            self.project.update(cx, |project, cx| {
                project.format(
                    [self.buffer.clone()].into_iter().collect(),
                    LspFormatTarget::Buffers,
                    true,
                    if options.force_format {
                        FormatTrigger::Manual
                    } else {
                        FormatTrigger::Save
                    },
                    cx,
                )
            })
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Some(formatting) = formatting {
                formatting.await?;
            }

            this.update_in(cx, |this, window, cx| {
                this.synchronize(ConflictEditReason::Edit, cx)?;
                this.refresh_blocks(window, cx);
                anyhow::Ok(this.persist(true, cx))
            })??
            .await
        })
    }

    fn apply_completed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.draft.conflicts.is_empty()
            && !self.draft.conflict.immutable
            && self.draft.applied_digest.is_none()
            && !self.applying
        {
            self.apply(false, window, cx);
        }
    }

    fn apply(&mut self, deleted: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.applying || self.draft.applied_digest.is_some() {
            return;
        }

        self.applying = true;
        self.error = None;
        let saved = self.save_draft(SaveOptions::default(), window, cx);
        let resolution_owner = cx.entity();
        cx.spawn_in(window, async move |this, cx| {
            // Closing the tab must not lose the applied marker after a successful publication.
            let _resolution_owner = resolution_owner;
            let result = async {
                saved.await?;
                let (submitted, write) = this.update(cx, |this, cx| {
                    this.synchronize(ConflictEditReason::Edit, cx)?;
                    ensure!(
                        deleted || this.draft.conflicts.is_empty(),
                        "Resolve every conflict block before applying"
                    );
                    let submitted = this.draft.text.clone();
                    let panel = this
                        .panel
                        .as_ref()
                        .and_then(WeakEntity::upgrade)
                        .or_else(|| this.workspace.upgrade()?.read(cx).panel::<KkPanel>(cx))
                        .context(
                            "KK Workbench is still loading. Refresh it before applying this draft",
                        )?;
                    let snapshot = match &this.snapshot {
                        Some(snapshot) => snapshot.clone(),
                        None => panel.read(cx).restored_conflict_snapshot(
                            &this.draft.conflict.repository_root,
                            this.operation.clone(),
                            cx,
                        )?,
                    };
                    let request = ConflictRequest {
                        snapshot,
                        conflict: this.draft.conflict.clone(),
                        result: (!deleted).then(|| submitted.as_bytes().to_vec()),
                        executable: None,
                    };
                    let write = panel.update(cx, |panel, cx| panel.resolve_conflict(request, cx));
                    anyhow::Ok((submitted, write))
                })??;
                let snapshot = write.await?;
                this.update(cx, |this, cx| {
                    this.operation = snapshot.operation().to_owned();
                    this.snapshot = Some(snapshot);
                    this.draft.mark_applied(&submitted, deleted);
                    this.persist(false, cx)
                })?
                .await
            }
            .await;
            this.update_in(cx, |this, window, cx| {
                this.applying = false;
                if let Err(error) = result {
                    this.error = Some(format!(
                        "Could not apply the resolution. Your draft is retained. {error:#}"
                    ));
                }

                this.refresh_blocks(window, cx);
                cx.notify();
            })
            .log_err();
        })
        .detach();
        cx.notify();
    }

    fn accept(
        &mut self,
        starts: Vec<usize>,
        choice: ConflictChoice,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.applying || self.draft.applied_digest.is_some() {
            return;
        }

        let result = (|| {
            self.synchronize(ConflictEditReason::Edit, cx)?;
            let acceptance = self.draft.prepare_accept(&starts, choice)?;
            let edits = acceptance.edits.clone();
            let mut next = self.draft.clone();
            next.accept(acceptance)?;
            self.editor.update(cx, |editor, cx| {
                let ranges = edits
                    .iter()
                    .map(|edit| {
                        MultiBufferOffset(edit.range.start)..MultiBufferOffset(edit.range.end)
                    })
                    .collect::<Vec<_>>();
                editor.unfold_ranges(&ranges, true, false, cx);
                if next.editor_text() != self.draft.editor_text() {
                    editor.finalize_last_transaction(cx);
                    editor.transact(window, cx, |editor, _, cx| {
                        editor.buffer().update(cx, |buffer, cx| {
                            buffer.edit(
                                edits.into_iter().map(|edit| {
                                    (
                                        MultiBufferOffset(edit.range.start)
                                            ..MultiBufferOffset(edit.range.end),
                                        edit.text,
                                    )
                                }),
                                None,
                                cx,
                            )
                        });
                    });
                    editor.finalize_last_transaction(cx);
                }
            });
            self.draft = next;
            anyhow::Ok(())
        })();
        if let Err(error) = result {
            self.error = Some(format!("{error:#}"));
        } else {
            self.error = None;
            self.apply_completed(window, cx);
        }

        self.refresh_blocks(window, cx);
        cx.emit(EditorEvent::DirtyChanged);
        cx.notify();
    }

    fn refresh_blocks(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        *self.line_numbers.borrow_mut() =
            (!self.draft.conflicts.is_empty()).then(|| self.draft.line_numbers().into());
        let mut blocks = Vec::new();
        let mut highlights = Vec::new();
        let text = self.editor.read(cx).buffer().read(cx).read(cx);
        let disabled = self.applying || self.draft.applied_digest.is_some();
        for conflict in &self.draft.conflicts {
            let start = conflict.range.start;
            if conflict.edited {
                highlights.push((
                    2,
                    text.anchor_before(MultiBufferOffset(start))
                        ..text.anchor_after(MultiBufferOffset(conflict.range.end)),
                ));
            } else {
                highlights.extend(
                    conflict
                        .sections
                        .iter()
                        .filter(|section| !section.is_empty_base())
                        .map(|section| {
                            let group = if section.role == ConflictRole::Base {
                                0
                            } else if section.index == 0 {
                                1
                            } else {
                                2
                            };
                            (
                                group,
                                text.anchor_before(MultiBufferOffset(section.range.start))
                                    ..text.anchor_after(MultiBufferOffset(section.range.end)),
                            )
                        }),
                );
            }

            let inputs: Vec<_> = if conflict.edited {
                vec![(start, "Edited block".to_owned(), ConflictChoice::Edited)]
            } else {
                conflict
                    .sections
                    .iter()
                    .filter(|section| !section.is_empty_base())
                    .map(|section| {
                        let title = self.input_title(section.role, section.index);
                        (
                            section.range.start,
                            title,
                            ConflictChoice::Input(section.role, section.index),
                        )
                    })
                    .collect()
            };
            for (offset, title, choice) in inputs {
                let weak = cx.weak_entity();
                let version = self.buffer.read(cx).version();
                let detail = match choice {
                    ConflictChoice::Input(role, index) => self
                        .draft
                        .conflict
                        .inputs
                        .iter()
                        .find(|input| input.role == role && input.index == index)
                        .map(VersionDelegate::details)
                        .unwrap_or_default(),
                    ConflictChoice::Edited => {
                        "Keep the current code in this block as the resolution.".into()
                    }
                };
                let change_id_prefix = if let ConflictChoice::Input(role, index) = choice {
                    self.draft
                        .conflict
                        .inputs
                        .iter()
                        .find(|input| input.role == role && input.index == index)
                        .and_then(|input| input.source.as_ref())
                        .map(|source| source.change_id_prefix.clone())
                } else {
                    None
                };
                let metadata = if let ConflictChoice::Input(role, index) = choice {
                    self.draft
                        .conflict
                        .inputs
                        .iter()
                        .find(|input| input.role == role && input.index == index)
                        .and_then(|input| input.source.as_ref())
                        .map(|source| {
                            vec![
                                (
                                    "Change ID".to_owned(),
                                    source.change_id.clone(),
                                    Color::Accent,
                                ),
                                (
                                    "Commit ID".to_owned(),
                                    source.commit_id.clone(),
                                    Color::Accent,
                                ),
                                (String::new(), source.description.clone(), Color::Default),
                                (String::new(), detail.clone(), Color::Muted),
                            ]
                        })
                } else {
                    None
                };
                let label = if choice == ConflictChoice::Edited {
                    "Use Edited Block"
                } else {
                    "Accept"
                };
                blocks.push(BlockProperties {
                    style: BlockStyle::Sticky,
                    placement: BlockPlacement::Above(text.anchor_before(MultiBufferOffset(offset))),
                    height: Some(1),
                    priority: 0,
                    render: Arc::new(move |cx| {
                        let weak = weak.clone();
                        let compare = weak.clone();
                        let version = version.clone();
                        h_flex()
                            .ml(cx.margins.gutter.width)
                            .h(cx.line_height)
                            .gap_2()
                            .child(Label::new(title.clone()).size(LabelSize::Small))
                            .child(
                                Button::new(("accept-conflict", offset), label)
                                    .disabled(disabled)
                                    .tooltip(super::commit_metadata::tooltip_with_prefix(metadata.clone().unwrap_or_else(|| vec![(String::new(), detail.clone(), Color::Muted)]), change_id_prefix.clone()))
                                    .on_click(move |_, window, cx| {
                                        weak.update(cx, |view, cx| {
                                            if view.buffer.read(cx).version() != version {
                                                view.error = Some("This conflict changed. Select the action again.".into());
                                                cx.notify();
                                                return;
                                            }

                                            view.accept(vec![start], choice, window, cx)
                                        })
                                        .log_err();
                                    }),
                            )
                            .when(matches!(choice, ConflictChoice::Input(role, index) if role != ConflictRole::Base || index != 0), |row| {
                                row.child(Button::new(("compare-input", offset), "Compare").on_click(move |_, window, cx| {
                                    if let ConflictChoice::Input(role, index) = choice {
                                        compare.update(cx, |view, cx| view.compare_input(role, index, window, cx)).log_err();
                                    }
                                }))
                            })
                            .into_any_element()
                    }),
                });
            }
        }

        drop(text);
        self.editor.update(cx, |editor, cx| {
            editor.clear_row_highlights::<BaseHighlight>();
            editor.clear_row_highlights::<FirstSideHighlight>();
            editor.clear_row_highlights::<OtherSideHighlight>();
            for (group, range) in highlights {
                match group {
                    0 => editor.highlight_rows::<BaseHighlight>(
                        range,
                        |cx| cx.theme().status().modified_background,
                        RowHighlightOptions::default(),
                        cx,
                    ),
                    1 => editor.highlight_rows::<FirstSideHighlight>(
                        range,
                        |cx| cx.theme().status().created_background,
                        RowHighlightOptions::default(),
                        cx,
                    ),
                    _ => editor.highlight_rows::<OtherSideHighlight>(
                        range,
                        |cx| cx.theme().status().conflict_background,
                        RowHighlightOptions::default(),
                        cx,
                    ),
                }
            }

            editor.remove_blocks(
                std::mem::take(&mut self.blocks).into_iter().collect(),
                None,
                cx,
            );
            self.blocks = editor.insert_blocks(blocks, None, cx).into_iter().collect();
            cx.notify();
        });
    }

    fn input_title(&self, role: ConflictRole, index: usize) -> String {
        self.draft
            .conflict
            .inputs
            .iter()
            .find(|input| input.role == role && input.index == index)
            .map(|input| version_title(&self.draft.conflict.inputs, input))
            .unwrap_or_else(|| "Unknown source".into())
    }

    fn navigate(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let editor = self.editor.read(cx);
        let offset = editor
            .selections
            .newest_anchor()
            .head()
            .to_offset(&editor.buffer().read(cx).read(cx))
            .0;
        let next = if forward {
            self.draft
                .conflicts
                .iter()
                .find(|conflict| conflict.range.start > offset)
                .or_else(|| self.draft.conflicts.first())
        } else {
            self.draft
                .conflicts
                .iter()
                .rev()
                .find(|conflict| conflict.range.start < offset)
                .or_else(|| self.draft.conflicts.last())
        };
        if let Some(conflict) = next {
            let start = conflict.range.start;
            self.editor.update(cx, |editor, cx| {
                editor.change_selections(Some(Autoscroll::fit()).into(), window, cx, |selection| {
                    selection.select_ranges([MultiBufferOffset(start)..MultiBufferOffset(start)])
                })
            });
        }
    }

    fn choose_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let inputs = self.draft.conflict.inputs.clone();
        let weak = cx.weak_entity();
        let version = self.buffer.read(cx).version();
        self.workspace
            .update(cx, |workspace, cx| {
                workspace.toggle_modal(window, cx, |window, cx| {
                    VersionPicker::new(inputs.clone(), window, cx, move |index, window, cx| {
                        if let Some(input) = inputs.get(index) {
                            weak.update(cx, |view, cx| {
                                if view.buffer.read(cx).version() != version {
                                    view.error = Some(
                                        "This conflict changed. Select the action again.".into(),
                                    );
                                    cx.notify();
                                    return;
                                }

                                view.accept(
                                    view.draft
                                        .conflicts
                                        .iter()
                                        .map(|conflict| conflict.range.start)
                                        .collect(),
                                    ConflictChoice::Input(input.role, input.index),
                                    window,
                                    cx,
                                )
                            })
                            .log_err();
                        }
                    })
                })
            })
            .log_err();
    }

    fn choose_comparison(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let inputs = self.draft.conflict.inputs.clone();
        let weak = cx.weak_entity();
        self.workspace
            .update(cx, |workspace, cx| {
                workspace.toggle_modal(window, cx, |window, cx| {
                    VersionPicker::new(inputs.clone(), window, cx, move |index, window, cx| {
                        if let Some(input) = inputs.get(index) {
                            weak.update(cx, |view, cx| {
                                view.compare_input(input.role, input.index, window, cx)
                            })
                            .log_err();
                        }
                    })
                })
            })
            .log_err();
    }

    fn compare_input(
        &mut self,
        role: ConflictRole,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let inputs = &self.draft.conflict.inputs;
        let Some(base) = inputs.iter().find(|input| input.role == ConflictRole::Base) else {
            return;
        };
        let Some(input) = inputs
            .iter()
            .find(|input| input.role == role && input.index == index)
        else {
            return;
        };
        let text = std::str::from_utf8(&input.bytes).map(str::to_owned);
        let base = std::str::from_utf8(&base.bytes).map(Arc::<str>::from);
        let title = format!(
            "{} · Base 1 ↔ {}",
            self.draft.conflict.path,
            self.input_title(role, index)
        );
        let path = self.draft.conflict.path.clone();
        let source = self.buffer.clone();
        let project = self.project.clone();
        let workspace = self.workspace.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = async {
                let workspace = workspace
                    .upgrade()
                    .ok_or_else(|| anyhow::anyhow!("Workspace was closed"))?;
                let text = text?;
                let base = base?;
                let buffer = cx.new(|cx| {
                    let mut buffer = Buffer::local(text, cx);
                    buffer.set_language_registry(project.read(cx).languages().clone());
                    buffer.set_language(source.read(cx).language().cloned(), cx);
                    buffer.set_capability(Capability::ReadOnly, cx);
                    buffer
                });
                let diff = cx.new(|cx| {
                    BufferDiff::new(
                        &buffer.read(cx).snapshot().text,
                        buffer.read(cx).language().cloned(),
                        buffer.read(cx).language_registry(),
                        cx,
                    )
                });
                diff.update(cx, |diff, cx| {
                    diff.set_base_text(Some(base.clone()), buffer.read(cx).snapshot().text, cx)
                })
                .await;
                workspace.update_in(cx, |workspace, window, cx| {
                    let owner = cx.entity();
                    let view = cx.new(|cx| {
                        super::kk_diff::KkDiff::conflict_comparison(
                            title, path, buffer, diff, base, project, owner, window, cx,
                        )
                    });
                    telemetry::event!("KK Conflict Comparison Opened");
                    workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
                })?;
                anyhow::Ok(())
            }
            .await;
            if let Err(error) = result {
                this.update(cx, |this, cx| {
                    this.error = Some(format!("Could not compare conflict versions: {error:#}"));
                    cx.notify();
                })
                .log_err();
            }
        })
        .detach();
    }

    fn export(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Task<()> {
        if let Err(error) = self.synchronize(ConflictEditReason::Edit, cx) {
            self.error = Some(error.to_string());
            cx.notify();
            return Task::ready(());
        }

        self.error = None;
        cx.notify();
        let suggested = format!(
            "{}.resolved",
            self.draft
                .conflict
                .path
                .rsplit('/')
                .next()
                .unwrap_or("resolution")
        );
        let remote = self.project.read(cx).kk_workbench_client(cx);
        let selected = if remote.is_some() {
            let Some(workspace) = self.workspace.upgrade() else {
                self.error = Some("Workspace was closed".into());
                cx.notify();
                return Task::ready(());
            };
            let selected = workspace.update(cx, |workspace, cx| {
                workspace.prompt_for_new_path(
                    DirectoryLister::ProjectAt {
                        project: self.project.clone(),
                        directory: self.draft.conflict.repository_root.clone(),
                    },
                    Some(suggested),
                    window,
                    cx,
                )
            });
            async move { anyhow::Ok(selected.await?.and_then(|paths| paths.into_iter().next())) }
                .boxed()
        } else {
            let selected =
                cx.prompt_for_new_path(&self.draft.conflict.repository_root, Some(&suggested));
            async move { selected.await? }.boxed()
        };
        let text = self.draft.text.clone();
        cx.spawn(async move |this, cx| {
            let result = async {
                if let Some(path) = selected.await? {
                    if let Some(remote) = remote {
                        remote.export_draft(path, text).await?;
                    } else {
                        cx.background_spawn(async move { std::fs::write(path, text) })
                            .await?;
                    }
                }

                anyhow::Ok(())
            }
            .await;
            if let Err(error) = result {
                this.update(cx, |this, cx| {
                    this.error = Some(format!("Could not export draft: {error:#}"));
                    cx.notify();
                })
                .log_err();
            }
        })
    }
}

impl EventEmitter<EditorEvent> for KkConflict {}
impl Focusable for KkConflict {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Render for KkConflict {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let status = if self.applying {
            "Applying resolution…".into()
        } else if self.draft.has_newer_edits() {
            "Applied · newer edits have not been applied".into()
        } else if self.draft.applied_digest.is_some() {
            "Resolution applied".into()
        } else if self.draft.conflict.immutable {
            "Immutable change · draft can be edited and exported".into()
        } else {
            format!("{} unresolved blocks", self.draft.conflicts.len())
        };
        v_flex()
            .size_full()
            .key_context("KKConflict")
            .on_action(
                cx.listener(|this, _: &NextConflict, window, cx| this.navigate(true, window, cx)),
            )
            .on_action(cx.listener(|this, _: &PreviousConflict, window, cx| {
                this.navigate(false, window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &AcceptAllConflictVersion, window, cx| {
                    this.choose_all(window, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &ApplyConflictResolution, window, cx| {
                    this.apply(false, window, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &DeleteConflictedFile, window, cx| {
                this.apply(true, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ExportConflictDraft, window, cx| {
                this.export(window, cx).detach()
            }))
            .on_action(
                cx.listener(|this, _: &CompareConflictVersions, window, cx| {
                    this.choose_comparison(window, cx)
                }),
            )
            .child(
                h_flex()
                    .p_1()
                    .gap_2()
                    .child(Label::new(status))
                    .child(
                        Button::new("compare-conflict", "Compare Versions…").on_click(
                            cx.listener(|this, _, window, cx| this.choose_comparison(window, cx)),
                        ),
                    )
                    .child(Button::new("previous-conflict", "Previous").on_click(
                        cx.listener(|this, _, window, cx| this.navigate(false, window, cx)),
                    ))
                    .child(Button::new("next-conflict", "Next").on_click(
                        cx.listener(|this, _, window, cx| this.navigate(true, window, cx)),
                    ))
                    .child(
                        Button::new("apply-conflict", "Apply Resolution")
                            .disabled(
                                self.applying
                                    || self.draft.conflict.immutable
                                    || self.draft.applied_digest.is_some()
                                    || !self.draft.conflicts.is_empty(),
                            )
                            .on_click(
                                cx.listener(|this, _, window, cx| this.apply(false, window, cx)),
                            ),
                    )
                    .child(
                        Button::new("accept-all-conflicts", "Accept All from Version…")
                            .disabled(self.applying || self.draft.applied_digest.is_some())
                            .on_click(
                                cx.listener(|this, _, window, cx| this.choose_all(window, cx)),
                            ),
                    )
                    .child(Button::new("export-conflict", "Export").on_click(
                        cx.listener(|this, _, window, cx| this.export(window, cx).detach()),
                    ))
                    .child(
                        Button::new("delete-conflict", "Delete File")
                            .disabled(
                                self.applying
                                    || self.draft.conflict.immutable
                                    || self.draft.applied_digest.is_some(),
                            )
                            .on_click(
                                cx.listener(|this, _, window, cx| this.apply(true, window, cx)),
                            ),
                    ),
            )
            .when_some(self.error.clone(), |view, error| {
                view.child(
                    h_flex()
                        .gap_2()
                        .child(Label::new(error).color(Color::Error))
                        .child(
                            Button::new("retry-conflict", "Retry")
                                .disabled(self.applying)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.apply_completed(window, cx)
                                })),
                        ),
                )
            })
            .child(div().flex_1().min_h_0().child(self.editor.clone()))
    }
}

impl Item for KkConflict {
    type Event = EditorEvent;
    fn tab_content_text(&self, _: usize, _: &App) -> SharedString {
        format!("{} (Resolve conflict)", self.draft.conflict.path).into()
    }
    fn tab_icon(&self, _: &Window, _: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Warning).color(Color::Warning))
    }
    fn to_item_events(event: &EditorEvent, callback: &mut dyn FnMut(ItemEvent)) {
        Editor::to_item_events(event, callback);
    }
    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("KK Conflict Opened")
    }
    fn act_as_type<'a>(
        &'a self,
        type_id: TypeId,
        handle: &'a Entity<Self>,
        _: &'a App,
    ) -> Option<gpui::AnyEntity> {
        if type_id == TypeId::of::<Self>() {
            Some(handle.clone().into())
        } else if type_id == TypeId::of::<Editor>() {
            Some(self.editor.clone().into())
        } else {
            None
        }
    }
    fn as_searchable(&self, _: &Entity<Self>, _: &App) -> Option<Box<dyn SearchableItemHandle>> {
        Some(Box::new(self.editor.clone()))
    }
    fn can_save(&self, _: &App) -> bool {
        true
    }
    fn is_dirty(&self, _: &App) -> bool {
        self.draft.is_dirty()
    }
    fn capability(&self, _: &App) -> Capability {
        Capability::ReadWrite
    }
    fn buffer_kind(&self, _: &App) -> ItemBufferKind {
        ItemBufferKind::Singleton
    }
    fn save(
        &mut self,
        options: SaveOptions,
        _: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.save_draft(options, window, cx)
    }
    fn reload(
        &mut self,
        _: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.draft.revert_to_saved();
        self.editor.update(cx, |editor, cx| {
            editor.set_text(self.draft.editor_text(), window, cx)
        });
        self.refresh_blocks(window, cx);
        Task::ready(Ok(()))
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
}

impl SerializableItem for KkConflict {
    fn serialized_item_kind() -> &'static str {
        "KKConflict"
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
        window.spawn(cx, async move |cx| {
            let restored = cx
                .background_spawn(async move {
                    let value = store
                        .scoped(&namespace)
                        .read(&item_id.to_string())?
                        .context("Saved conflict tab is unavailable")?;
                    let mut saved: SavedConflictTab = serde_json::from_str(&value)?;
                    ensure!(
                        saved.host == host,
                        "This conflict draft belongs to another project host"
                    );
                    if let Some(value) = store.read_kvp(&saved.key)? {
                        let latest: StoredConflictDraft = serde_json::from_str(&value)?;
                        if latest.saved_version > saved.saved_version {
                            saved.draft = latest.draft.clone();
                        } else {
                            saved.draft.acknowledge_save(&latest.draft);
                            saved.draft.applied_digest = latest.draft.applied_digest.clone();
                            saved.draft.applied_deleted = latest.draft.applied_deleted;
                            Self::refresh_metadata(&mut saved.draft, &latest.draft.conflict);
                        }

                        saved.saved_draft = latest.draft;
                        saved.saved_version = latest.saved_version;
                    }

                    anyhow::Ok(saved)
                })
                .await?;
            let buffer = Self::draft_buffer(&restored.draft, &project, cx).await?;
            let panel = workspace.read_with(cx, |workspace, cx| {
                workspace
                    .panel::<KkPanel>(cx)
                    .map(|panel| panel.downgrade())
            })?;
            cx.update(|window, cx| {
                cx.new(|cx| {
                    let mut view = Self::new(
                        None,
                        restored.draft,
                        restored.key,
                        buffer,
                        project,
                        workspace,
                        panel,
                        window,
                        cx,
                    );
                    view.saved_draft = restored.saved_draft;
                    view.saved_version = restored.saved_version;
                    view.operation = restored.operation;
                    view.refresh_blocks(window, cx);
                    view.editor.update(cx, |editor, cx| {
                        restored.editor.restore(editor, window, cx);
                    });
                    view
                })
            })
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
        let store = KeyValueStore::global(cx);
        let restore_unsaved = project::project_settings::ProjectSettings::get_global(cx)
            .session
            .restore_unsaved_buffers;
        let previous = self.writes.clone();
        let owner = cx.entity();
        self.writes = cx
            .spawn(async move |this, cx| {
                let _owner = owner;
                if let Err(error) = previous.await {
                    log::warn!("Earlier KK conflict persistence failed: {error}");
                }

                let saved = this
                    .update(cx, |this, cx| {
                        this.synchronize(ConflictEditReason::Edit, cx)?;
                        let editor = this.editor.update(cx, SerializedEditorState::capture);
                        anyhow::Ok(SavedConflictTab {
                            key: this.key.clone(),
                            draft: if restore_unsaved {
                                this.draft.clone()
                            } else {
                                this.saved_snapshot()
                            },
                            saved_draft: this.saved_snapshot(),
                            saved_version: this.saved_version,
                            operation: this.operation.clone(),
                            host: this
                                .project
                                .read(cx)
                                .remote_connection_options(cx)
                                .map(|options| options.host_key()),
                            editor,
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
                .map_err(|error| {
                    format!("Could not restore this conflict tab next session: {error:#}")
                })
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

fn version_title(inputs: &[ConflictInput], input: &ConflictInput) -> String {
    let count = inputs
        .iter()
        .enumerate()
        .filter(|(index, input)| {
            input.role == ConflictRole::Base
                && !inputs
                    .iter()
                    .take(*index)
                    .any(|other| input.same_base(other))
        })
        .count();
    let title = input.title(count);
    if input.role == ConflictRole::Side
        && let Some(source) = &input.source
        && inputs.iter().any(|other| {
            other.role == input.role && other.index != input.index && other.title(count) == title
        })
    {
        format!(
            "{title} · {} · input {}",
            source.commit_id.chars().take(8).collect::<String>(),
            input.index + 1
        )
    } else {
        title
    }
}

struct VersionPicker {
    picker: Entity<Picker<VersionDelegate>>,
    pending_choice: Option<Rc<RefCell<Option<futures::channel::oneshot::Sender<usize>>>>>,
    _subscription: Subscription,
}
type ChooseVersion = Box<dyn Fn(usize, &mut Window, &mut App)>;
struct VersionDelegate {
    inputs: Vec<ConflictInput>,
    matches: Vec<usize>,
    selected: usize,
    confirm: ChooseVersion,
}

impl VersionPicker {
    fn choose(
        inputs: Vec<ConflictInput>,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Task<Option<usize>> {
        if workspace.active_modal::<Self>(cx).is_some() && !workspace.hide_modal(window, cx) {
            return Task::ready(None);
        }

        let (selected, selection) = futures::channel::oneshot::channel();
        let selected = Rc::new(RefCell::new(Some(selected)));
        workspace.toggle_modal(window, cx, move |window, cx| {
            let pending_choice = selected.clone();
            let mut picker = Self::new(inputs, window, cx, move |index, _, _| {
                if let Some(selected) = selected.borrow_mut().take()
                    && selected.send(index).is_err()
                {
                    log::debug!("Conflict version selection was cancelled");
                }
            });
            picker.pending_choice = Some(pending_choice);
            picker
        });
        cx.spawn(async move |_, _| match selection.await {
            Ok(index) => Some(index),
            Err(error) => {
                log::debug!("Conflict version picker dismissed: {error}");
                None
            }
        })
    }

    fn new(
        inputs: Vec<ConflictInput>,
        window: &mut Window,
        cx: &mut Context<Self>,
        confirm: impl Fn(usize, &mut Window, &mut App) + 'static,
    ) -> Self {
        let picker = cx.new(|cx| {
            Picker::uniform_list(
                VersionDelegate {
                    matches: (0..inputs.len()).collect(),
                    inputs,
                    selected: 0,
                    confirm: Box::new(confirm),
                },
                window,
                cx,
            )
            .initial_width(rems(48.))
            .reopenable(false, cx)
        });
        let subscription =
            cx.subscribe(&picker, |_, _, _: &DismissEvent, cx| cx.emit(DismissEvent));
        Self {
            picker,
            pending_choice: None,
            _subscription: subscription,
        }
    }
}
impl ModalView for VersionPicker {
    fn on_before_dismiss(
        &mut self,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> workspace::DismissDecision {
        if let Some(pending) = self.pending_choice.take() {
            drop(pending.borrow_mut().take());
        }

        workspace::DismissDecision::Dismiss(true)
    }
}
impl EventEmitter<DismissEvent> for VersionPicker {}
impl Focusable for VersionPicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}
impl Render for VersionPicker {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        self.picker.clone()
    }
}

impl VersionDelegate {
    fn title(&self, input: &ConflictInput) -> String {
        version_title(&self.inputs, input)
    }
    fn details(input: &ConflictInput) -> String {
        let source = input
            .source
            .as_ref()
            .map(|source| {
                format!(
                    "{} {} · {}",
                    source.change_id,
                    source.commit_id,
                    source.description.trim()
                )
            })
            .unwrap_or_else(|| "Source unavailable".into());
        let kind = if input.id.is_none() {
            "File absent".into()
        } else {
            format!(
                "{} bytes{}",
                input.bytes.len(),
                if input.executable {
                    " · executable"
                } else {
                    ""
                }
            )
        };
        format!("{source} · {kind}")
    }
}
impl PickerDelegate for VersionDelegate {
    type ListItem = ListItem;
    fn name() -> &'static str {
        "KK conflict version picker"
    }
    fn placeholder_text(&self, _: &mut Window, _: &mut App) -> Arc<str> {
        "Choose a conflict version by source, bookmark, or description…".into()
    }
    fn match_count(&self) -> usize {
        self.matches.len()
    }
    fn selected_index(&self) -> usize {
        self.selected
    }
    fn set_selected_index(&mut self, index: usize, _: &mut Window, _: &mut Context<Picker<Self>>) {
        self.selected = index;
    }
    fn update_matches(
        &mut self,
        query: String,
        _: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let query = query.to_lowercase();
        self.matches = self
            .inputs
            .iter()
            .enumerate()
            .filter_map(|(index, input)| {
                format!("{} {}", self.title(input), Self::details(input))
                    .to_lowercase()
                    .contains(&query)
                    .then_some(index)
            })
            .collect();
        self.selected = 0;
        cx.notify();
        Task::ready(())
    }
    fn confirm(&mut self, _: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        if let Some(index) = self.matches.get(self.selected) {
            (self.confirm)(*index, window, cx);
            cx.emit(DismissEvent);
        }
    }
    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        cx.emit(DismissEvent);
    }
    fn render_match(
        &self,
        index: usize,
        selected: bool,
        _: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let input = self.inputs.get(*self.matches.get(index)?)?;
        Some(
            ListItem::new(index)
                .inset(true)
                .toggle_state(selected)
                .child(
                    v_flex()
                        .child(Label::new(self.title(input)))
                        .when_some(input.source.as_ref(), |view, source| {
                            view.child(
                                h_flex()
                                    .gap_1()
                                    .child(
                                        super::commit_metadata::change_identifier(
                                            source
                                                .display_change_id
                                                .as_ref()
                                                .unwrap_or(&source.change_id)
                                                .clone(),
                                            &source.change_id_prefix,
                                        )
                                        .size(LabelSize::Small),
                                    )
                                    .child(
                                        super::commit_metadata::identifier(
                                            source.commit_id.clone(),
                                        )
                                        .size(LabelSize::Small),
                                    )
                                    .child(
                                        super::commit_metadata::description(
                                            source.description.trim().to_owned(),
                                        )
                                        .size(LabelSize::Small)
                                        .truncate(),
                                    ),
                            )
                        })
                        .child(
                            Label::new(if input.id.is_none() {
                                "File absent".to_owned()
                            } else {
                                format!(
                                    "{} bytes{}",
                                    input.bytes.len(),
                                    if input.executable {
                                        " · executable"
                                    } else {
                                        ""
                                    }
                                )
                            })
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                        )
                        .when(input.source.is_none(), |view| {
                            view.child(Label::new("Source unavailable").color(Color::Muted))
                        }),
                ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use editor::SplittableEditor;
    use gpui::{TestAppContext, VisualContext, VisualTestContext};
    use language::Point;
    use project::FakeFs;
    use settings::SettingsStore;
    use std::path::Path;
    use workspace::MultiWorkspace;

    #[gpui::test]
    async fn binary_version_selection_waits_for_choice_or_modal_cancellation(
        cx: &mut TestAppContext,
    ) {
        cx.update(|cx| {
            let settings = SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/repository", serde_json::json!({})).await;
        let project = Project::test(fs, [Path::new("/repository")], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = multi_workspace.read_with(cx, |workspace, _| workspace.workspace().clone());
        let inputs = vec![ConflictInput {
            role: ConflictRole::Side,
            index: 1,
            id: Some("blob".into()),
            executable: true,
            bytes: vec![0, 255],
            source: None,
        }];
        let mut chosen = workspace.update_in(cx, |workspace, window, cx| {
            VersionPicker::choose(inputs.clone(), workspace, window, cx)
        });
        cx.run_until_parked();
        assert!((&mut chosen).now_or_never().is_none());
        let picker = workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_modal::<VersionPicker>(cx)
                .expect("version picker")
                .read(cx)
                .picker
                .clone()
        });
        picker.update_in(cx, |picker, window, cx| {
            picker.delegate.confirm(false, window, cx)
        });
        cx.run_until_parked();
        assert_eq!(chosen.now_or_never(), Some(Some(0)));
        drop(picker);
        cx.run_until_parked();
        let mut cancelled = workspace.update_in(cx, |workspace, window, cx| {
            VersionPicker::choose(inputs.clone(), workspace, window, cx)
        });
        cx.run_until_parked();
        assert!((&mut cancelled).now_or_never().is_none());
        let mut replacement = workspace.update_in(cx, |workspace, window, cx| {
            VersionPicker::choose(inputs, workspace, window, cx)
        });
        cx.run_until_parked();
        assert_eq!(cancelled.now_or_never(), Some(None));
        assert!((&mut replacement).now_or_never().is_none());
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.hide_modal(window, cx);
        });
        cx.run_until_parked();
        assert_eq!(replacement.now_or_never(), Some(None));
    }

    #[gpui::test(iterations = 3)]
    async fn kk_remote_binary_picker_releases_split_before_waiting_for_selection(
        cx: &mut TestAppContext,
        server_cx: &mut TestAppContext,
    ) {
        use crate::{
            kk_comparison::{Comparison, ComparisonIntent},
            kk_panel::remote_tests::{RemoteFixture, run_kk},
        };
        use kk_workbench::{Category, ClassificationTarget, FileSelection, RepositoryLifetime};

        let fixture = RemoteFixture::create(cx, server_cx)
            .await
            .expect("remote fixture");
        cx.update(editor::init);
        let remote = fixture
            .project
            .read_with(cx, |project, cx| project.kk_workbench_client(cx))
            .expect("transport");
        let repository = remote
            .open(&fixture.root, Path::new("kk"), Default::default())
            .await
            .expect("repository");
        std::fs::write(fixture.root.join("binary"), b"left\0").expect("left input");
        let left = repository
            .snapshot(true)
            .await
            .expect("left snapshot")
            .exact_revision("@")
            .await
            .expect("left");
        run_kk(&fixture.root, &["new", "@-"])
            .await
            .expect("other side");
        std::fs::write(fixture.root.join("binary"), b"right\0").expect("right input");
        run_kk(
            &fixture.root,
            &["rebase", "-r", "@", "-d", &left.full_commit_id],
        )
        .await
        .expect("binary conflict");
        let conflict_revision = repository
            .snapshot(true)
            .await
            .expect("conflict snapshot")
            .exact_revision("@")
            .await
            .expect("conflict revision");
        run_kk(&fixture.root, &["new", "@--"])
            .await
            .expect("independent work");
        std::fs::write(fixture.root.join("fresh.txt"), "classify me\n").expect("working file");
        let snapshot = repository
            .snapshot(true)
            .await
            .expect("snapshot")
            .with_lifetime(Some(RepositoryLifetime::default()));
        let parent = snapshot.exact_revision("@-").await.expect("parent");
        let comparison = Comparison::load(
            &snapshot,
            ComparisonIntent::for_target(parent.revset(), "base".into(), None),
        )
        .await
        .expect("comparison");
        let file = comparison
            .files
            .iter()
            .find(|file| file.path == "fresh.txt")
            .expect("file")
            .clone();
        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            MultiWorkspace::test_new(fixture.project.clone(), window, cx)
        });
        let workspace = multi_workspace.read_with(cx, |view, _| view.workspace().clone());
        let panel = cx
            .update(|window, cx| KkPanel::load(workspace.downgrade(), window.to_async(cx)))
            .await
            .expect("panel");
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_panel(panel.clone(), window, cx)
        });
        panel
            .update(cx, |panel, cx| panel.refresh_for_test(cx))
            .await;
        let gate = snapshot
            .repository()
            .begin_diff_opening()
            .await
            .expect("read gate");
        let classified = panel.update(cx, |panel, cx| {
            panel.classify_files(
                snapshot.clone(),
                comparison.diff_target(),
                comparison.intent,
                vec![FileSelection { file, ranges: None }],
                ClassificationTarget::Category(Category::Reviewed),
                cx,
            )
        });
        let opened = cx.update(|window, cx| {
            KkConflict::open(
                snapshot,
                conflict_revision.full_commit_id.clone(),
                "binary".into(),
                panel.downgrade(),
                workspace.downgrade(),
                window,
                cx,
            )
        });
        let mut opened = std::pin::pin!(opened);
        let selected = workspace.condition::<workspace::Event>(cx, |workspace, cx| {
            workspace.active_modal::<VersionPicker>(cx).is_some()
        });
        let timeout = cx
            .background_executor
            .timer(std::time::Duration::from_secs(30));
        let selected = async move {
            assert!(
                matches!(
                    futures::future::select(std::pin::pin!(selected), std::pin::pin!(timeout))
                        .await,
                    futures::future::Either::Left(_)
                ),
                "Binary picker did not open"
            );
        };
        match futures::future::select(opened.as_mut(), std::pin::pin!(selected)).await {
            futures::future::Either::Left((result, _)) => {
                panic!("Expected a binary version picker: {result:?}")
            }
            futures::future::Either::Right(_) => {}
        }
        drop(gate);
        let timeout = cx
            .background_executor
            .timer(std::time::Duration::from_secs(30));
        assert!(
            matches!(
                futures::future::select(std::pin::pin!(classified), std::pin::pin!(timeout)).await,
                futures::future::Either::Left(_)
            ),
            "Split must complete while the binary picker remains open"
        );
        let picker = workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_modal::<VersionPicker>(cx)
                .expect("picker remains open")
                .read(cx)
                .picker
                .clone()
        });
        let (index, expected) = picker.read_with(cx, |picker, _| {
            picker
                .delegate
                .inputs
                .iter()
                .enumerate()
                .find(|(_, input)| input.id.is_some() && input.role == ConflictRole::Side)
                .map(|(index, input)| (index, input.bytes.clone()))
                .expect("binary side")
        });
        picker.update_in(cx, |picker, window, cx| {
            picker.delegate.selected = picker
                .delegate
                .matches
                .iter()
                .position(|candidate| *candidate == index)
                .expect("matching side");
            picker.delegate.confirm(false, window, cx)
        });
        let timeout = cx
            .background_executor
            .timer(std::time::Duration::from_secs(30));
        match futures::future::select(std::pin::pin!(opened), std::pin::pin!(timeout)).await {
            futures::future::Either::Left((result, _)) => result.expect("resolution completed"),
            futures::future::Either::Right(_) => panic!("Binary resolution was blocked by Split"),
        }
        let current = repository.snapshot(false).await.expect("current snapshot");
        assert_eq!(
            current
                .file_content(&conflict_revision.revset(), "binary")
                .await
                .expect("resolved file"),
            expected
        );
        let range = current.exact_revision("@").await.expect("working copy");
        assert!(
            current
                .category_commits(&range)
                .await
                .expect("categories")
                .iter()
                .any(|entry| entry.category == Category::Reviewed
                    && entry.files.iter().any(|file| file.path == "fresh.txt"))
        );
    }

    #[gpui::test(iterations = 3)]
    async fn kk_remote_conflict_exports_through_host_paths(
        cx: &mut TestAppContext,
        server_cx: &mut TestAppContext,
    ) {
        run_remote_conflict_export(cx, server_cx)
            .await
            .expect("remote conflict export");
    }

    async fn run_remote_conflict_export(
        cx: &mut TestAppContext,
        server_cx: &mut TestAppContext,
    ) -> Result<()> {
        use crate::kk_panel::remote_tests::{RemoteFixture, run_kk};
        use anyhow::Context as _;
        let fixture = RemoteFixture::create(cx, server_cx).await?;
        let project = fixture.project.clone();
        let other_directory = tempfile::tempdir()?;
        let original_worktree = project
            .read_with(cx, |project, cx| {
                project
                    .visible_worktrees(cx)
                    .next()
                    .map(|tree| tree.read(cx).id())
            })
            .context("original worktree")?;
        project
            .update(cx, |project, cx| {
                project.find_or_create_worktree(other_directory.path(), true, cx)
            })
            .await?;
        project.update(cx, |project, cx| {
            project.remove_worktree(original_worktree, cx)
        });
        project
            .update(cx, |project, cx| {
                project.find_or_create_worktree(&fixture.root, true, cx)
            })
            .await?;
        project.read_with(cx, |project, cx| {
            assert_ne!(
                project
                    .visible_worktrees(cx)
                    .next()
                    .expect("first worktree")
                    .read(cx)
                    .abs_path()
                    .as_ref(),
                fixture.root.as_path()
            );
        });
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
        let left = repository.snapshot(true).await?.exact_revision("@").await?;
        run_kk(&fixture.root, &["new", "@-"]).await?;
        std::fs::write(fixture.root.join("file.txt"), "other side\n")?;
        run_kk(
            &fixture.root,
            &["rebase", "-r", "@", "-d", &left.full_commit_id],
        )
        .await?;
        let snapshot = repository.snapshot(true).await?;
        let original = std::fs::read(fixture.root.join("file.txt"))?;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |view, _| view.workspace().clone());
        let panel = cx
            .update(|window, cx| KkPanel::load(workspace.downgrade(), window.to_async(cx)))
            .await?;
        cx.update(|window, cx| {
            KkConflict::open(
                snapshot,
                "@".into(),
                "file.txt".into(),
                panel.downgrade(),
                workspace.downgrade(),
                window,
                cx,
            )
        })
        .await?;
        let view = workspace
            .read_with(cx, |workspace, cx| {
                workspace.items_of_type::<KkConflict>(cx).next()
            })
            .context("remote conflict editor")?;
        view.update(cx, |view, cx| {
            view.buffer.update(cx, |buffer, cx| {
                buffer.edit([(0..0, "exported draft\n")], None, cx);
            });
            view.synchronize(ConflictEditReason::Edit, cx)
        })?;
        let text = view.read_with(cx, |view, _| view.draft.text.clone());
        let target = fixture.root.join("exported.resolved");
        for selected in [
            Some(target.clone()),
            None,
            Some(fixture.root.join("missing/file")),
        ] {
            let expected = selected.clone();
            let root = fixture.root.clone();
            workspace.update(cx, |workspace, _| {
                workspace.set_prompt_for_new_path(Box::new(move |_, lister, suggested, _, cx| {
                    assert!(!lister.is_local(cx));
                    assert_eq!(lister.default_query(cx), format!("{}/", root.display()));
                    assert_eq!(suggested.as_deref(), Some("file.txt.resolved"));
                    let (sender, receiver) = futures::channel::oneshot::channel();
                    sender
                        .send(selected.clone().map(|path| vec![path]))
                        .expect("path selection");
                    receiver
                }));
            });
            view.update_in(cx, |view, window, cx| view.export(window, cx))
                .await;
            let failed = expected.as_ref().is_some_and(|path| path != &target);
            view.read_with(cx, |view, _| {
                assert_eq!(view.error.is_some(), failed, "{:?}", view.error)
            });
            assert_eq!(std::fs::read(&target)?, text.as_bytes());
            assert_eq!(std::fs::read(fixture.root.join("file.txt"))?, original);
        }

        Ok(())
    }

    async fn open_test_conflict(
        snapshot: Snapshot,
        revision: &str,
        panel: &Entity<KkPanel>,
        workspace: &Entity<Workspace>,
        cx: &mut VisualTestContext,
    ) -> Result<Entity<KkConflict>> {
        cx.update(|window, cx| {
            KkConflict::open(
                snapshot,
                revision.into(),
                "file.txt".into(),
                panel.downgrade(),
                workspace.downgrade(),
                window,
                cx,
            )
        })
        .await?;
        workspace
            .read_with(cx, |workspace, cx| {
                workspace.active_item_as::<KkConflict>(cx)
            })
            .ok_or_else(|| anyhow::anyhow!("active conflict editor"))
    }

    #[gpui::test(iterations = 3)]
    async fn kk_remote_conflict_reopens_saved_drafts_and_applies_explicitly(
        cx: &mut TestAppContext,
        server_cx: &mut TestAppContext,
    ) {
        run_remote_conflict_lifecycle(cx, server_cx)
            .await
            .expect("remote conflict lifecycle");
    }

    async fn run_remote_conflict_lifecycle(
        cx: &mut TestAppContext,
        server_cx: &mut TestAppContext,
    ) -> Result<()> {
        use crate::kk_panel::remote_tests::{RemoteFixture, run_kk};
        use anyhow::Context as _;
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
        let left = repository.snapshot(true).await?.exact_revision("@").await?;
        run_kk(&fixture.root, &["new", "@-"]).await?;
        std::fs::write(fixture.root.join("file.txt"), "other side\n")?;
        run_kk(
            &fixture.root,
            &["rebase", "-r", "@", "-d", &left.full_commit_id],
        )
        .await?;
        std::fs::write(fixture.root.join("notes.txt"), "original notes\n")?;
        let revision = repository
            .snapshot(true)
            .await?
            .exact_revision("@")
            .await?
            .full_change_id;
        run_kk(&fixture.root, &["new", "-m", "descendant"]).await?;
        let snapshot = repository.snapshot(true).await?;
        let original = std::fs::read(fixture.root.join("file.txt"))?;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |view, _| view.workspace().clone());
        let panel = cx
            .update(|window, cx| KkPanel::load(workspace.downgrade(), window.to_async(cx)))
            .await?;
        panel
            .update(cx, |panel, cx| panel.refresh_for_test(cx))
            .await;
        let view = open_test_conflict(snapshot.clone(), &revision, &panel, &workspace, cx).await?;
        view.read_with(cx, |view, cx| {
            assert!(view.buffer.read(cx).peek_undo_stack().is_none());
        });
        let editor = view.read_with(cx, |view, _| view.editor.clone());
        editor.update_in(cx, |editor, window, cx| {
            editor.buffer().update(cx, |buffer, cx| {
                buffer.edit([(Point::zero()..Point::zero(), "saved prefix\n")], None, cx);
            });
            editor.focus_handle(cx).focus(window, cx);
        });
        view.update_in(cx, |view, window, cx| {
            view.save_draft(SaveOptions::default(), window, cx)
        })
        .await?;
        let saved = view.read_with(cx, |view, _| view.draft.clone());
        let saved_key = view.read_with(cx, |view, _| view.key.clone());
        editor.update(cx, |editor, cx| {
            editor.buffer().update(cx, |buffer, cx| {
                buffer.edit(
                    [(Point::zero()..Point::zero(), "discarded prefix\n")],
                    None,
                    cx,
                );
            });
        });
        cx.run_until_parked();
        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
        let closing = pane.update_in(cx, |pane, window, cx| {
            pane.close_item_by_id(view.entity_id(), workspace::SaveIntent::Close, window, cx)
        });
        cx.run_until_parked();
        assert!(cx.has_pending_prompt());
        cx.simulate_prompt_answer("Don't Save");
        closing.await?;
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert!(!view.is_dirty(cx));
            assert_eq!(view.draft.text, saved.text);
            assert_eq!(view.draft.conflicts, saved.conflicts);
            assert_eq!(view.buffer.read(cx).text(), saved.editor_text());
        });
        let reopened =
            open_test_conflict(snapshot.clone(), &revision, &panel, &workspace, cx).await?;
        assert_ne!(reopened.entity_id(), view.entity_id());
        reopened.read_with(cx, |view, _| {
            assert_eq!(view.draft.text, saved.text);
            assert_eq!(view.draft.conflicts, saved.conflicts);
            assert_eq!(view.key, saved_key);
        });
        assert_eq!(std::fs::read(fixture.root.join("file.txt"))?, original);

        run_kk(
            &fixture.root,
            &[
                "config",
                "set",
                "--repo",
                r#"revset-aliases."immutable_heads()""#,
                &revision,
            ],
        )
        .await?;
        let immutable =
            open_test_conflict(snapshot.clone(), &revision, &panel, &workspace, cx).await?;
        assert_eq!(immutable.entity_id(), reopened.entity_id());
        immutable.read_with(cx, |view, _| assert!(view.draft.conflict.immutable));
        immutable.update_in(cx, |view, window, cx| {
            let starts = view
                .draft
                .conflicts
                .iter()
                .map(|block| block.range.start)
                .collect();
            view.accept(
                starts,
                ConflictChoice::Input(ConflictRole::Side, 1),
                window,
                cx,
            );
            assert!(view.draft.conflicts.is_empty());
            assert!(!view.applying);
        });
        immutable
            .update_in(cx, |view, window, cx| {
                view.save_draft(SaveOptions::default(), window, cx)
            })
            .await?;
        assert_eq!(std::fs::read(fixture.root.join("file.txt"))?, original);

        let notes_path = project
            .read_with(cx, |project, cx| {
                project.find_project_path(&fixture.root.join("notes.txt"), cx)
            })
            .context("notes path")?;
        let notes = project
            .update(cx, |project, cx| project.open_buffer(notes_path, cx))
            .await?;
        notes.update(cx, |buffer, cx| {
            buffer.edit([(0..0, "new notes\n")], None, cx)
        });
        run_kk(
            &fixture.root,
            &[
                "config",
                "set",
                "--repo",
                r#"revset-aliases."immutable_heads()""#,
                "none()",
            ],
        )
        .await?;
        let mutable =
            open_test_conflict(snapshot.clone(), &revision, &panel, &workspace, cx).await?;
        assert_eq!(mutable.entity_id(), reopened.entity_id());
        cx.condition(&mutable, |view, _| !view.applying).await;
        mutable.read_with(cx, |view, _| {
            assert!(!view.draft.conflict.immutable);
            assert!(view.draft.applied_digest.is_none());
            assert!(
                view.error
                    .as_ref()
                    .is_some_and(|error| error.contains("notes.txt")),
                "{:?}",
                view.error
            );
        });
        project
            .update(cx, |project, cx| project.save_buffer(notes.clone(), cx))
            .await?;
        cx.update(|window, cx| {
            mutable.focus_handle(cx).focus(window, cx);
            window.refresh();
            window.draw(cx).clear(cx);
        });
        cx.dispatch_action(ApplyConflictResolution);
        cx.condition(&mutable, |view, _| !view.applying).await;
        let submitted = mutable.read_with(cx, |view, _| {
            assert!(view.error.is_none(), "{:?}", view.error);
            assert!(view.draft.applied_digest.is_some());
            view.draft.text.clone()
        });
        assert_eq!(
            std::fs::read(fixture.root.join("file.txt"))?,
            submitted.as_bytes()
        );
        assert_eq!(
            std::fs::read(fixture.root.join("notes.txt"))?,
            b"new notes\noriginal notes\n"
        );
        let published_operation = repository.snapshot(false).await?.operation().to_owned();
        cx.dispatch_action(ApplyConflictResolution);
        cx.run_until_parked();
        assert_eq!(
            repository.snapshot(false).await?.operation(),
            published_operation
        );

        mutable.update(cx, |view, cx| {
            view.buffer.update(cx, |buffer, cx| {
                buffer.edit([(0..0, "newer result\n")], None, cx)
            });
        });
        cx.run_until_parked();
        run_kk(&fixture.root, &["undo"]).await?;
        panel
            .update(cx, |panel, cx| panel.refresh_for_test(cx))
            .await;
        let fresh = open_test_conflict(
            repository.snapshot(false).await?,
            &revision,
            &panel,
            &workspace,
            cx,
        )
        .await?;
        assert_ne!(fresh.entity_id(), mutable.entity_id());
        fresh.read_with(cx, |view, _| {
            assert!(view.draft.applied_digest.is_none());
            assert!(!view.draft.conflicts.is_empty());
        });
        mutable.read_with(cx, |view, _| {
            assert!(view.draft.has_newer_edits());
            assert!(view.draft.text.starts_with("newer result\n"));
        });

        run_kk(&fixture.root, &["edit", &revision]).await?;
        std::fs::write(fixture.root.join("file.txt"), "external resolution\n")?;
        repository.snapshot(true).await?;
        fresh.update_in(cx, |view, window, cx| {
            let starts = view
                .draft
                .conflicts
                .iter()
                .map(|block| block.range.start)
                .collect();
            view.accept(
                starts,
                ConflictChoice::Input(ConflictRole::Side, 0),
                window,
                cx,
            );
        });
        cx.condition(&fresh, |view, _| !view.applying).await;
        fresh.read_with(cx, |view, _| {
            assert!(view.draft.applied_digest.is_none());
            assert!(
                view.error
                    .as_ref()
                    .is_some_and(|error| error.contains("changed")),
                "{:?}",
                view.error
            );
            assert!(view.draft.conflicts.is_empty());
        });
        assert_eq!(
            std::fs::read(fixture.root.join("file.txt"))?,
            b"external resolution\n"
        );

        pane.update_in(cx, |pane, window, cx| {
            pane.close_item_by_id(fresh.entity_id(), workspace::SaveIntent::Skip, window, cx)
        })
        .await?;
        let item_count = workspace.read_with(cx, |workspace, cx| workspace.items(cx).count());
        let lifetime = kk_workbench::RepositoryLifetime::default();
        let created = Rc::new(std::cell::Cell::new(false));
        let _close_on_buffer_creation = cx.update(|_, cx| {
            let lifetime = lifetime.clone();
            let created = created.clone();
            cx.observe_new::<Buffer>(move |buffer, _, _| {
                if buffer.file().is_none() {
                    created.set(true);
                    lifetime.invalidate();
                }
            })
        });
        let result = open_test_conflict(
            snapshot.with_lifetime(Some(lifetime)),
            &revision,
            &panel,
            &workspace,
            cx,
        )
        .await;
        assert!(
            created.get(),
            "close while the remote draft buffer is being created"
        );
        assert!(
            result.is_err(),
            "a closed repository cannot open a late conflict tab"
        );
        assert_eq!(
            workspace.read_with(cx, |workspace, cx| workspace.items(cx).count()),
            item_count
        );
        Ok(())
    }

    #[gpui::test(iterations = 3)]
    async fn kk_remote_conflict_restores_saved_and_unsaved_session_versions(
        cx: &mut TestAppContext,
        server_cx: &mut TestAppContext,
    ) {
        run_conflict_session_restore(cx, server_cx)
            .await
            .expect("restore conflict session");
    }

    async fn run_conflict_session_restore(
        cx: &mut TestAppContext,
        server_cx: &mut TestAppContext,
    ) -> Result<()> {
        use crate::kk_panel::remote_tests::{RemoteFixture, run_kk};
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
        let left = repository.snapshot(true).await?.exact_revision("@").await?;
        run_kk(&fixture.root, &["new", "@-"]).await?;
        std::fs::write(fixture.root.join("file.txt"), "other side\n")?;
        run_kk(
            &fixture.root,
            &["rebase", "-r", "@", "-d", &left.full_commit_id],
        )
        .await?;
        let snapshot = repository.snapshot(true).await?;
        let operation = snapshot.operation().to_owned();
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
            view.activate(workspace.clone(), None, window, cx);
        });
        let panel = cx
            .update(|window, cx| KkPanel::load(workspace.downgrade(), window.to_async(cx)))
            .await?;
        panel
            .update(cx, |panel, cx| panel.refresh_for_test(cx))
            .await;
        let view = open_test_conflict(snapshot.clone(), "@", &panel, &workspace, cx).await?;
        view.update(cx, |view, cx| {
            view.buffer.update(cx, |buffer, cx| {
                buffer.edit([(0..0, "saved prefix\n")], None, cx)
            });
        });
        view.update_in(cx, |view, window, cx| {
            view.save_draft(SaveOptions::default(), window, cx)
        })
        .await?;
        view.update_in(cx, |view, window, cx| {
            view.buffer.update(cx, |buffer, cx| {
                buffer.edit([(0..0, "unsaved prefix\n")], None, cx)
            });
            view.editor.update(cx, |editor, cx| {
                editor.change_selections(
                    editor::SelectionEffects::no_scroll(),
                    window,
                    cx,
                    |selections| {
                        selections.select_ranges([Point::new(1, 5)..Point::new(0, 2)]);
                    },
                );
            });
        });
        let handle = view
            .read_with(cx, |_, cx| view.to_serializable_item_handle(cx))
            .context("registered serializable conflict")?;
        workspace
            .update(cx, |workspace, cx| handle.serialize(workspace, false, cx))
            .context("serialize conflict")?
            .await?;
        let store = cx.update(|_, cx| KeyValueStore::global(cx));
        let namespace = session_namespace(workspace_id);
        let item_id = view.entity_id().as_u64();
        let old_record = store
            .scoped(&namespace)
            .read(&item_id.to_string())?
            .context("session record")?;
        let record: serde_json::Value = serde_json::from_str(&old_record)?;
        assert_eq!(record["selections"], serde_json::json!([[[1, 5], [0, 2]]]));
        assert!(record.get("scroll").is_some());
        assert!(record.get("editor").is_none());
        let draft_key = view.read_with(cx, |view, _| view.key.clone());
        let canonical = store.read_kvp(&draft_key)?.context("saved draft")?;
        assert!(old_record.contains("unsaved prefix"));
        assert!(!canonical.contains("unsaved prefix"));
        let expected = view.read_with(cx, |view, _| view.draft.clone());
        let restored_workspace = cx.new_window_entity(|window, cx| {
            Workspace::new(
                Some(workspace_id),
                project.clone(),
                app_state.clone(),
                window,
                cx,
            )
        });
        let restored = cx
            .update(|window, cx| {
                KkConflict::deserialize(
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
            assert!(view.is_dirty(cx));
            assert!(view.snapshot.is_none());
            assert!(view.panel.is_none());
            assert_eq!(view.draft.text, expected.text);
            assert_eq!(view.draft.saved_text, expected.saved_text);
            assert_eq!(view.draft.conflicts, expected.conflicts);
            assert!(view.editor.read(cx).selections.newest_anchor().reversed);
        });
        assert_eq!(repository.snapshot(false).await?.operation(), operation);
        assert_eq!(std::fs::read(fixture.root.join("file.txt"))?, original);

        let independent_id = database.next_id().await?;
        let independent_workspace = cx.new_window_entity(|window, cx| {
            Workspace::new(
                Some(independent_id),
                project.clone(),
                app_state.clone(),
                window,
                cx,
            )
        });
        let independent =
            open_test_conflict(snapshot, "@", &panel, &independent_workspace, cx).await?;
        independent.read_with(cx, |view, cx| {
            assert_ne!(view.key, draft_key);
            assert!(!view.is_dirty(cx));
            assert!(!view.draft.text.contains("saved prefix"));
        });

        view.update(cx, |view, cx| {
            view.buffer.update(cx, |buffer, cx| {
                buffer.edit([(0..0, "new saved version\n")], None, cx)
            });
        });
        view.update_in(cx, |view, window, cx| {
            view.save_draft(SaveOptions::default(), window, cx)
        })
        .await?;
        store
            .scoped(&namespace)
            .write(item_id.to_string(), old_record)
            .await?;
        let latest = cx
            .update(|window, cx| {
                KkConflict::deserialize(
                    project.clone(),
                    restored_workspace.downgrade(),
                    workspace_id,
                    item_id,
                    window,
                    cx,
                )
            })
            .await?;
        latest.read_with(cx, |view, cx| {
            assert!(!view.is_dirty(cx));
            assert!(view.draft.text.starts_with("new saved version\n"));
        });

        cx.update(|_, cx| {
            cx.update_global::<SettingsStore, _>(|settings, cx| {
                settings.set_user_settings(r#"{"kk_workbench":{"auto_refresh":false},"session":{"restore_unsaved_buffers":false}}"#, cx).expect("saved-only session settings");
            });
        });
        view.update(cx, |view, cx| {
            view.buffer.update(cx, |buffer, cx| {
                buffer.edit([(0..0, "not restored\n")], None, cx)
            });
        });
        workspace
            .update(cx, |workspace, cx| handle.serialize(workspace, false, cx))
            .context("serialize saved-only conflict")?
            .await?;
        let saved_only = cx
            .update(|window, cx| {
                KkConflict::deserialize(
                    project.clone(),
                    restored_workspace.downgrade(),
                    workspace_id,
                    item_id,
                    window,
                    cx,
                )
            })
            .await?;
        saved_only.read_with(cx, |view, cx| {
            assert!(!view.is_dirty(cx));
            assert!(!view.draft.text.contains("not restored"));
        });
        cx.update(|window, cx| KkConflict::cleanup(workspace_id, Vec::new(), window, cx))
            .await?;
        assert!(store.scoped(&namespace).keys()?.is_empty());
        assert!(store.read_kvp(&draft_key)?.is_some());
        Ok(())
    }

    #[gpui::test(iterations = 3)]
    async fn kk_conflict_native_undo_and_draft_save(cx: &mut TestAppContext) {
        cx.background_executor.allow_parking();
        let (directory, snapshot, mut draft) = cx
            .background_spawn(async {
                futures_lite::future::block_on(async {
                    let directory = tempfile::tempdir()?;
                    let run = |arguments: Vec<String>| {
                        let root = directory.path().to_owned();
                        async move {
                            let output = util::command::new_command("kk")
                                .current_dir(root)
                                .args(arguments)
                                .output()
                                .await?;
                            ensure!(
                                output.status.success(),
                                "{}",
                                String::from_utf8_lossy(&output.stderr)
                            );
                            anyhow::Ok(String::from_utf8(output.stdout)?)
                        }
                    };
                    for arguments in [
                        vec!["init", "."],
                        vec!["config", "set", "--repo", "user.name", "Conflict Test"],
                        vec![
                            "config",
                            "set",
                            "--repo",
                            "user.email",
                            "test@example.invalid",
                        ],
                    ] {
                        run(arguments.into_iter().map(str::to_owned).collect()).await?;
                    }

                    std::fs::write(directory.path().join("file.txt"), b"common\r\nbase\r\n")?;
                    let base = run(["log", "-r", "@", "--no-graph", "-T", "commit_id"]
                        .map(str::to_owned)
                        .into())
                    .await?;
                    run(vec!["new".into()]).await?;
                    std::fs::write(directory.path().join("file.txt"), b"common\r\nleft\r\n")?;
                    let left = run(["log", "-r", "@", "--no-graph", "-T", "commit_id"]
                        .map(str::to_owned)
                        .into())
                    .await?;
                    run(vec!["new".into(), base]).await?;
                    std::fs::write(directory.path().join("file.txt"), b"common\r\nright\r\n")?;
                    run(vec![
                        "rebase".into(),
                        "-r".into(),
                        "@".into(),
                        "-d".into(),
                        left,
                    ])
                    .await?;
                    let repository =
                        kk_workbench::Repository::open(directory.path(), Path::new("kk")).await?;
                    let snapshot = repository.snapshot(true).await?;
                    let draft =
                        ConflictDraft::open(snapshot.file_conflict("@", "file.txt").await?)?;
                    anyhow::Ok((directory, snapshot, draft))
                })
            })
            .await
            .expect("isolated conflict");
        draft.conflict.immutable = true;
        cx.update(|cx| {
            let settings = SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let root = snapshot.repository().root().to_owned();
        let workspace_contents =
            std::fs::read(root.join("file.txt")).expect("initial workspace conflict");
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(&root, serde_json::json!({"file.txt": "workspace contents"}))
            .await;
        let project = Project::test(fs, [root.as_path()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |workspace, _| workspace.workspace().clone());
        let panel = cx
            .update(|window, cx| KkPanel::load(workspace.downgrade(), window.to_async(cx)))
            .await
            .expect("panel");
        let initial = draft.editor_text();
        let buffer = cx.new(|cx| Buffer::local(initial.clone(), cx));
        let key = format!("test-conflict:{}", uuid::Uuid::new_v4());
        let view = cx.new_window_entity(|window, cx| {
            KkConflict::new(
                Some(snapshot.clone()),
                draft,
                key.clone(),
                buffer.clone(),
                project.clone(),
                workspace.downgrade(),
                Some(panel.downgrade()),
                window,
                cx,
            )
        });
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.add_item_to_active_pane(Box::new(view.clone()), None, true, window, cx)
            })
        });
        view.update_in(cx, |view, window, cx| {
            view.compare_input(ConflictRole::Side, 1, window, cx);
        });
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(view.error.is_none(), "{:?}", view.error)
        });
        workspace.read_with(cx, |workspace, cx| {
            let comparison = workspace
                .items_of_type::<super::super::kk_diff::KkDiff>(cx)
                .next()
                .expect("native version comparison");
            let comparison = (&comparison as &dyn workspace::ItemHandle)
                .act_as::<SplittableEditor>(cx)
                .expect("comparison editor");
            let comparison = comparison.read(cx);
            assert_eq!(comparison.capability(cx), Capability::ReadOnly);
            assert!(!comparison.can_save(cx));
            assert!(!comparison.can_save_as(cx));
            assert!(!comparison.is_dirty(cx));
            assert!(comparison.rhs_editor().read(cx).read_only(cx));
        });
        view.update_in(cx, |view, window, cx| {
            view.refresh_blocks(window, cx);
            assert_eq!(view.blocks.len(), 3);
            let start = view.draft.conflicts[0].range.start;
            view.accept(
                vec![start],
                ConflictChoice::Input(ConflictRole::Side, 0),
                window,
                cx,
            );
            assert!(view.draft.conflicts.is_empty());
            assert!(view.error.is_none(), "{:?}", view.error);
            assert!(!view.applying);
        });
        cx.run_until_parked();
        let accepted = buffer.read_with(cx, |buffer, _| buffer.text());
        assert!(!accepted.contains("<<<<<<<"));
        let editor = view.read_with(cx, |view, _| view.editor.clone());
        editor.update_in(cx, |editor, window, cx| {
            editor.undo(&editor::actions::Undo, window, cx)
        });
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.draft.conflicts.len(), 1);
            assert_eq!(view.draft.editor_text(), initial);
            assert!(!view.draft.conflicts[0].edited);
        });
        editor.update_in(cx, |editor, window, cx| {
            editor.redo(&editor::actions::Redo, window, cx)
        });
        cx.run_until_parked();
        view.read_with(cx, |view, _| assert!(view.draft.conflicts.is_empty()));
        view.update_in(cx, |view, window, cx| {
            view.save_draft(SaveOptions::default(), window, cx)
        })
        .await
        .expect("save draft");
        assert!(!view.read_with(cx, |view, _| view.draft.is_dirty()));
        let stored = cx.read(|cx| {
            KeyValueStore::global(cx)
                .read_kvp(&key)
                .expect("read draft")
                .expect("stored draft")
        });
        let saved: ConflictDraft = serde_json::from_str(&stored).expect("decode draft");
        assert_eq!(saved.editor_text(), accepted);
        assert!(saved.text.contains("\r\n"));
        assert_eq!(
            std::fs::read(directory.path().join("file.txt")).expect("workspace conflict"),
            workspace_contents
        );
        let submitted = saved.text.clone();
        buffer.update(cx, |buffer, cx| {
            buffer.edit([(0..0, "newer draft\n")], None, cx)
        });
        cx.run_until_parked();
        view.update(cx, |view, cx| {
            view.draft.mark_applied(&submitted, false);
            view.persist(false, cx)
        })
        .await
        .expect("persist applied metadata");
        view.read_with(cx, |view, _| {
            assert!(view.draft.has_newer_edits());
            assert!(view.draft.is_dirty());
        });
        let stored = cx.read(|cx| {
            KeyValueStore::global(cx)
                .read_kvp(&key)
                .expect("read metadata")
                .expect("stored metadata")
        });
        let saved: ConflictDraft = serde_json::from_str(&stored).expect("decode metadata");
        assert!(
            !stored.contains("newer draft"),
            "metadata writes must not persist unsaved text or history"
        );
        assert_eq!(saved.text, submitted);
        assert!(saved.applied_digest.is_some());

        panel
            .update(cx, |panel, cx| panel.refresh_for_test(cx))
            .await;
        cx.update(|window, cx| {
            KkConflict::open(
                snapshot.clone(),
                "@".into(),
                "file.txt".into(),
                panel.downgrade(),
                workspace.downgrade(),
                window,
                cx,
            )
        })
        .await
        .expect("open mutable conflict");
        let mutable = workspace
            .read_with(cx, |workspace, cx| {
                workspace
                    .items_of_type::<KkConflict>(cx)
                    .find(|view| view.read(cx).draft.applied_digest.is_none())
            })
            .expect("new resolution session");
        assert_ne!(mutable.entity_id(), view.entity_id());
        mutable.update_in(cx, |view, window, cx| {
            let starts = view
                .draft
                .conflicts
                .iter()
                .map(|conflict| conflict.range.start)
                .collect();
            view.accept(
                starts,
                ConflictChoice::Input(ConflictRole::Side, 1),
                window,
                cx,
            );
        });
        cx.condition(&mutable, |view, _| !view.applying).await;
        mutable.read_with(cx, |view, _| {
            assert!(view.error.is_none(), "{:?}", view.error);
            assert!(view.draft.applied_digest.is_some());
        });
        let repository = snapshot.repository().clone();
        let conflicts = cx
            .background_spawn(async move {
                repository
                    .snapshot(false)
                    .await?
                    .conflicted_paths("@")
                    .await
            })
            .await
            .expect("resolved paths");
        assert!(conflicts.is_empty());
        assert!(
            !std::fs::read_to_string(root.join("file.txt"))
                .expect("resolved working file")
                .contains("<<<<<<<")
        );
        view.read_with(cx, |view, _| {
            assert!(view.draft.is_dirty());
            assert!(view.draft.text.starts_with("newer draft"));
        });
    }
}
