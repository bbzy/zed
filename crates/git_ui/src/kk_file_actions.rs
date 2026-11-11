use super::*;
use std::collections::{BTreeSet, HashSet};

#[derive(Clone, Copy)]
pub(super) enum FileAction {
    Open(FileOpenKind),
    Mutate(FileMutationAction),
    Classify(ClassificationTarget),
}

impl FileAction {
    fn label(self) -> &'static str {
        match self {
            Self::Open(FileOpenKind::Before) => "Open File Before Change",
            Self::Open(_) => "Open File",
            Self::Mutate(action) => action.label(),
            Self::Classify(ClassificationTarget::Category(category)) => category.label(),
            Self::Classify(ClassificationTarget::Comparison) => "Comparison",
        }
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub(super) struct FileBatchKey {
    action: &'static str,
    sequence: u64,
    rows: BTreeSet<FileRowId>,
    lifetime: Option<kk_workbench::RepositoryLifetime>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) struct FileRowId {
    pub(super) root: PathBuf,
    pub(super) section: String,
    pub(super) source: String,
    pub(super) path: String,
}

#[derive(Clone)]
pub(super) struct FileRow {
    pub id: FileRowId,
    pub(super) snapshot: Snapshot,
    pub(super) target: super::super::kk_diff::DiffTarget,
    pub(super) file: ChangedFile,
    pub(super) working_copy: bool,
    pub(super) can_move_parent: bool,
    pub(super) can_move_child: bool,
    pub(super) category: Option<Category>,
    pub(super) pending: bool,
    pub(super) pending_action: Option<&'static str>,
    pub(super) classification_dependency: Option<WriteDependency>,
    pub(super) capture_saved: bool,
}

impl FileRow {
    pub(super) fn new(
        repository: &RepositoryView,
        mut target: super::super::kk_diff::DiffTarget,
        file: ChangedFile,
        section: &str,
        working_copy: bool,
    ) -> Self {
        let comparison = repository
            .comparison
            .as_ref()
            .filter(|comparison| match &target.scope {
                DiffScope::Change(commit) => comparison.sources.contains(commit),
                DiffScope::Comparison { .. } => true,
            });
        let category = comparison.and_then(|comparison| comparison.categories.iter().find(|category| {
            matches!(&target.scope, DiffScope::Change(commit) if *commit == category.revision.full_commit_id)
        })).map(|category| category.category);
        target.comparison = comparison.map(|comparison| comparison.intent.clone());
        let revisions = repository
            .history
            .revisions
            .iter()
            .chain(repository.located.iter())
            .chain(repository.comparison.iter().flat_map(|comparison| {
                comparison
                    .categories
                    .iter()
                    .map(|category| &category.revision)
            }))
            .collect::<Vec<_>>();
        let source = revisions.iter().find(|revision| matches!(&target.scope, DiffScope::Change(commit) if *commit == revision.full_commit_id));
        let mut source_identity = target.identity.clone();
        if let Some(source) = source
            && revisions
                .iter()
                .filter(|revision| revision.full_change_id == source.full_change_id)
                .map(|revision| &revision.full_commit_id)
                .collect::<HashSet<_>>()
                .len()
                > 1
        {
            source_identity.push_str(&format!(":{}", source.full_commit_id));
        }

        let section = if section == "category" {
            format!("category:{}", category.map_or("Unknown", Category::label))
        } else {
            format!("{section}:{source_identity}")
        };
        Self {
            id: FileRowId {
                root: repository.snapshot.repository().root().to_owned(),
                section,
                source: source_identity,
                path: file.path.clone(),
            },
            snapshot: repository.snapshot.clone(),
            target,
            file,
            working_copy,
            can_move_parent: source
                .is_some_and(|source| !source.is_root && source.parent_commit_ids.len() == 1),
            can_move_child: source.is_some_and(|source| {
                !source.has_multiple_children
                    && revisions
                        .iter()
                        .any(|revision| revision.parent_commit_ids.contains(&source.full_commit_id))
            }),
            category,
            pending: false,
            pending_action: None,
            classification_dependency: None,
            capture_saved: true,
        }
    }
}

impl KkPanel {
    pub(super) fn run_file_action(
        &mut self,
        action: FileAction,
        row: FileRow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.run_file_batch(action, vec![row], window, cx);
    }

    pub(super) fn run_selected_files(
        &mut self,
        action: FileAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let clicked = self
            .tree_selection
            .focused_file()
            .and_then(|anchor| self.visible_files.iter().find(|row| row.id == *anchor))
            .cloned();
        let Some(clicked) = clicked else {
            return;
        };
        match self.selected_file_rows(&clicked) {
            Ok(rows) => self.run_file_batch(action, rows, window, cx),
            Err(error) => self.show_error(error.to_string(), cx),
        }
    }

    fn batch_file_menu(
        &mut self,
        rows: Vec<FileRow>,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(first) = rows.first() else {
            return;
        };
        let mut actions = vec![FileAction::Open(FileOpenKind::Current)];
        let available = !first.pending || first.pending_action.is_some();
        if available && matches!(first.target.scope, DiffScope::Change(_)) {
            actions.push(FileAction::Open(FileOpenKind::Before));
        }

        if available && first.working_copy {
            actions.push(FileAction::Mutate(FileMutationAction::Revert));
        }

        if available && first.can_move_parent {
            actions.push(FileAction::Mutate(FileMutationAction::Move(
                FileMoveDirection::Parent,
            )));
        }

        if available && first.can_move_child {
            actions.push(FileAction::Mutate(FileMutationAction::Move(
                FileMoveDirection::Child,
            )));
        }

        if first.target.comparison.is_some() {
            for category in [
                Category::Pending,
                Category::Reviewed,
                Category::Rejected,
                Category::Debug,
            ] {
                if first.category != Some(category) {
                    actions.push(FileAction::Classify(ClassificationTarget::Category(
                        category,
                    )));
                }
            }

            if first.category.is_some() {
                actions.push(FileAction::Classify(ClassificationTarget::Comparison));
            }
        }

        let panel = cx.weak_entity();
        let menu = ContextMenu::build(window, cx, |mut menu, _, _| {
            for action in actions {
                let panel = panel.clone();
                let rows = rows.clone();
                menu = menu.entry(
                    format!("KKW: {}", action.label()),
                    None,
                    move |window, cx| {
                        panel
                            .update(cx, |panel, cx| {
                                panel.run_file_batch(action, rows.clone(), window, cx)
                            })
                            .log_err();
                    },
                );
            }

            menu
        });
        self.show_context_menu(menu, position, window, cx);
    }

    fn file_action_ineligible(
        &self,
        action: FileAction,
        row: &FileRow,
        cx: &App,
    ) -> Option<&'static str> {
        if row.pending
            && row.pending_action.is_none()
            && !matches!(
                action,
                FileAction::Open(FileOpenKind::Current) | FileAction::Classify(_)
            )
        {
            return Some("This classification is awaiting confirmation.");
        }

        if let FileAction::Open(kind) = action {
            return (kind == FileOpenKind::Before
                && !matches!(row.target.scope, DiffScope::Change(_)))
            .then_some("This file has no change to open before.");
        }

        if self.file_is_dirty(&row.id.root, &row.file, cx) {
            return Some("Save or discard unsaved editor changes first.");
        }

        match action {
            FileAction::Mutate(FileMutationAction::Revert) if !row.working_copy => {
                Some("This file cannot be reverted.")
            }
            FileAction::Mutate(FileMutationAction::Move(FileMoveDirection::Parent))
                if !row.can_move_parent =>
            {
                Some("This file has no unique parent to move to.")
            }
            FileAction::Mutate(FileMutationAction::Move(FileMoveDirection::Child))
                if !row.can_move_child =>
            {
                Some("This file has no unique child to move to.")
            }
            FileAction::Classify(_) if row.target.comparison.is_none() => {
                Some("Open a comparison before classifying this file.")
            }
            FileAction::Classify(ClassificationTarget::Comparison) if row.category.is_none() => {
                Some("Only category changes can be returned to Comparison.")
            }
            FileAction::Classify(ClassificationTarget::Category(category))
                if row.category == Some(category) =>
            {
                Some("This file is already in the selected category.")
            }
            FileAction::Classify(_) if row.file.has_conflict => {
                Some("Resolve this file conflict before classifying it.")
            }
            _ => None,
        }
    }

    fn run_file_batch(
        &mut self,
        action: FileAction,
        rows: Vec<FileRow>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(failure_root) = rows.first().map(|row| row.id.root.clone()) else {
            return;
        };
        let sequence = if matches!(action, FileAction::Classify(_)) {
            self.classification_batch_sequence += 1;
            self.classification_batch_sequence
        } else {
            0
        };
        let key = FileBatchKey {
            action: action.label(),
            sequence,
            rows: rows.iter().map(|row| row.id.clone()).collect(),
            lifetime: rows
                .first()
                .and_then(|row| row.snapshot.repository().lifetime())
                .cloned(),
        };
        if self.file_batches.contains_key(&key) {
            return;
        }

        let mut failures = Vec::new();
        let eligible = rows
            .into_iter()
            .filter_map(|mut row| {
                match self.bind_snapshot(row.snapshot.clone(), cx) {
                    Ok(snapshot) => row.snapshot = snapshot,
                    Err(error) => {
                        failures.push(format!("{}: {error:#}", row.file.path));
                        return None;
                    }
                }

                if let Some(reason) = self.file_action_ineligible(action, &row, cx) {
                    failures.push(format!("{}: {reason}", row.file.path));
                    return None;
                }

                let mut dependencies = if matches!(action, FileAction::Mutate(_)) {
                    let paths = std::iter::once(row.file.path.clone())
                        .chain(row.file.renamed_from.clone())
                        .collect::<Vec<_>>();
                    self.write_queues
                        .get(&row.id.root)
                        .map(|queue| {
                            queue.dependencies(&paths, row.snapshot.repository().lifetime())
                        })
                        .unwrap_or_default()
                } else {
                    Vec::new()
                };
                if matches!(action, FileAction::Mutate(_))
                    && let Some(dependency) = &row.classification_dependency
                {
                    dependencies.push(dependency.clone());
                }
                Some((row, dependencies))
            })
            .collect::<Vec<_>>();
        let prompt = (matches!(action, FileAction::Mutate(FileMutationAction::Revert))
            && !eligible.is_empty())
        .then(|| {
            let details = eligible
                .iter()
                .map(|(row, _)| {
                    let source = match &row.target.scope {
                        DiffScope::Comparison { base, .. } => base.clone(),
                        DiffScope::Change(_) => format!("parents({})", row.target.identity),
                    };
                    format!("{} → {source}", row.file.path)
                })
                .collect::<Vec<_>>()
                .join("\n");
            window.prompt(
                gpui::PromptLevel::Warning,
                &format!(
                    "Revert {} selected file(s)? This will discard their current changes.",
                    eligible.len()
                ),
                Some(&details),
                &["Revert Files", "Cancel"],
                cx,
            )
        });
        let task_key = key.clone();
        let opening = if matches!(action, FileAction::Open(_)) {
            eligible
                .first()
                .map(|(row, _)| self.file_opening.begin(&row.snapshot))
        } else {
            None
        };
        let preparation = opening.as_ref().and_then(|opening| {
            let (first, _) = eligible.first()?;
            let paths = eligible
                .iter()
                .filter(|(row, _)| {
                    !row.file.has_conflict
                        && matches!(action, FileAction::Open(FileOpenKind::Current))
                })
                .map(|(row, _)| row.file.path.clone())
                .collect();
            Some(self.prepare_open(first.snapshot.clone(), paths, opening, window, cx))
        });
        let feedback = matches!(action, FileAction::Open(_)).then(|| {
            let rows = eligible
                .iter()
                .map(|(row, _)| TreeRowId::File(row.id.clone()))
                .collect();
            let label = if let Some((row, _)) = eligible.first().filter(|_| eligible.len() == 1) {
                format!("Opening file: {}…", row.file.path)
            } else {
                format!("Opening {} files…", eligible.len())
            };
            (label, rows)
        });
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = async {
                if let Some(prompt) = prompt {
                    if prompt.await? != 0 {
                        return anyhow::Ok(());
                    }
                }

                let mut succeeded = 0;
                if let FileAction::Open(kind) = action {
                    if let Some(preparation) = preparation && preparation.await.map_err(|error| anyhow::anyhow!(error.to_string()))?.is_none() {
                        return Ok(());
                    }

                    let Some(opening) = opening else { return Ok(()); };
                    for (row, _) in eligible {
                        if !opening.is_current() {
                            return Ok(());
                        }

                        let result = this.update_in(cx, |panel, window, cx| {
                            if !panel.repository_is_open(&row.id.root, cx) || row.snapshot.repository().check_lifetime().is_err() {
                                return Task::ready(Err(anyhow::anyhow!("The selected repository was closed")));
                            }

                            panel.open_prepared_file_task(row.snapshot.clone(), row.target.clone(), row.file.clone(), row.working_copy, kind, opening.clone(), window, cx)
                        })?;
                        let result = opening.wait(result).await;
                        match result { Ok(Some(())) => succeeded += 1, Ok(None) => return Ok(()), Err(error) => failures.push(format!("{}: {error:#}", row.file.path)) }
                    }
                } else {
                    let jobs = this.update(cx, |panel, cx| eligible.into_iter().map(|(row, dependencies)| {
                        let task = if !panel.repository_is_open(&row.id.root, cx) || panel.uncertain_operations.contains_key(&row.id.root) {
                            Task::ready(Err(ClassificationFailure { message: "The repository was closed or its previous operation is unconfirmed. Refresh before retrying.".into(), uncertain: false }))
                        } else {
                            match action {
                                FileAction::Mutate(action) => panel.queue_file_mutation_result(FileMutationRequest { snapshot: row.snapshot, target: row.target, file: row.file.clone(), action }, dependencies, cx),
                                FileAction::Classify(category) => {
                                    if let Some(intent) = row.target.comparison.clone() {
                                        let files = vec![FileSelection { file: row.file.clone(), ranges: None }];
                                        if row.pending {
                                            let source = row.pending_action.is_none().then(|| row.category.map(ClassificationTarget::Category).unwrap_or(ClassificationTarget::Comparison));
                                            panel.queue_classification_from(row.snapshot, row.target, intent, files, category, Vec::new(), source, row.classification_dependency, row.capture_saved, cx)
                                        } else {
                                            panel.classify_files_result(row.snapshot, row.target, intent, files, category, cx)
                                        }
                                    } else {
                                        Task::ready(Err(ClassificationFailure { message: "The comparison was closed".into(), uncertain: false }))
                                    }
                                }
                                FileAction::Open(_) => Task::ready(Err(ClassificationFailure { message: "Use the ordered file-opening path for this action".into(), uncertain: false })),
                            }
                        };
                        (row.file.path, task)
                    }).collect::<Vec<_>>())?;
                    for (path, task) in jobs {
                        match task.await { Ok(_) => succeeded += 1, Err(error) => failures.push(format!("{path}: {}", error.message)) }
                    }
                }

                this.update(cx, |panel, cx| {
                    if !failures.is_empty() {
                        let message = format!("{} — {succeeded} file(s) completed; {} failed or skipped.\n{}", action.label(), failures.len(), failures.join("\n"));
                        if matches!(action, FileAction::Open(_)) {
                            panel.show_error(message, cx);
                        } else {
                            panel.operation_error = Some((failure_root, message));
                            cx.notify();
                        }
                    }

                    if !matches!(action, FileAction::Open(_)) {
                        panel.refresh(cx);
                    }
                })?;
                Ok(())
            }.await;
            this.update(cx, |panel, cx| {
                panel.file_batches.remove(&task_key);
                if let Err(error) = result {
                    panel.show_error(format!("{}: {error:#}", action.label()), cx);
                }

                cx.notify();
            }).log_err();
        });
        let task = if let Some((label, rows)) = feedback {
            let task = cx.spawn(async move |_, _| {
                task.await;
                anyhow::Ok(())
            });
            let task = self.track_opening(label, Some(rows), task, cx);
            cx.spawn(async move |_, _| {
                task.await.log_err();
            })
        } else {
            task
        };
        self.file_batches.insert(key, task);
    }

    pub(super) fn selected_file_rows(&self, clicked: &FileRow) -> anyhow::Result<Vec<FileRow>> {
        anyhow::ensure!(
            !self.tree_selection.contains_file(&clicked.id)
                || self
                    .tree_selection
                    .selected
                    .iter()
                    .all(|row| matches!(row, TreeRowId::File(_))),
            "Select files from one comparison, commit, or category in one repository."
        );
        let rows = if self.tree_selection.contains_file(&clicked.id) {
            self.visible_files
                .iter()
                .filter(|row| self.tree_selection.contains_file(&row.id))
                .cloned()
                .collect::<Vec<_>>()
        } else {
            vec![clicked.clone()]
        };
        anyhow::ensure!(
            rows.iter()
                .all(|row| row.id.root == clicked.id.root && row.id.section == clicked.id.section),
            "Select files from one comparison, commit, or category in one repository."
        );
        Ok(rows)
    }

    pub(super) fn show_file_actions(
        &mut self,
        clicked: FileRow,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let rows = match self.selected_file_rows(&clicked) {
            Ok(rows) => rows,
            Err(error) => {
                self.show_error(error.to_string(), cx);
                return;
            }
        };
        if rows.len() < 2 && !clicked.pending {
            if let Some(index) = self
                .repositories
                .iter()
                .position(|repository| repository.snapshot.repository().root() == clicked.id.root)
            {
                self.file_menu(
                    index,
                    clicked.target,
                    clicked.file,
                    clicked.working_copy,
                    position,
                    window,
                    cx,
                );
            }

            return;
        }

        self.batch_file_menu(rows, position, window, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kk_panel::remote_tests::run_kk;
    use gpui::TestAppContext;
    use project::{FakeFs, Project};
    use workspace::MultiWorkspace;

    #[gpui::test(iterations = 3)]
    async fn batches_open_in_display_order_and_confirm_once_preserving_later_drafts(
        cx: &mut TestAppContext,
    ) {
        cx.background_executor.allow_parking();
        let (directory, snapshot, history, files) = cx
            .background_spawn(async {
                futures_lite::future::block_on(async {
                    let directory = tempfile::tempdir()?;
                    run_kk(directory.path(), &["init", "."]).await?;
                    run_kk(
                        directory.path(),
                        &["config", "set", "--repo", "user.name", "KK Test"],
                    )
                    .await?;
                    run_kk(
                        directory.path(),
                        &[
                            "config",
                            "set",
                            "--repo",
                            "user.email",
                            "test@example.invalid",
                        ],
                    )
                    .await?;
                    for path in ["z10.txt", "z2.txt", "a.txt"] {
                        std::fs::write(directory.path().join(path), format!("{path} base\n"))?;
                    }

                    run_kk(directory.path(), &["new"]).await?;
                    for path in ["z10.txt", "z2.txt", "a.txt"] {
                        std::fs::write(directory.path().join(path), format!("{path} changed\n"))?;
                    }

                    let repository =
                        kk_workbench::Repository::open(directory.path(), Path::new("kk")).await?;
                    let snapshot = repository.snapshot(true).await?;
                    let history = snapshot.history(HistoryMode::Lineage, 50).await?;
                    let files = snapshot
                        .changed_files(&DiffScope::Change("@".into()))
                        .await?;
                    anyhow::Ok((directory, snapshot, history, files))
                })
            })
            .await
            .expect("prepare batch repository");
        let root = snapshot.repository().root().to_owned();
        cx.update(|cx| {
            let settings = SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(&root, serde_json::json!({"a.txt": "a.txt changed\n", "z2.txt": "z2.txt changed\n", "z10.txt": "z10.txt changed\n"})).await;
        let project = Project::test(fs, [root.as_path()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |view, _| view.workspace().clone());
        workspace.update_in(cx, |workspace, window, cx| {
            let status =
                cx.new(|cx| super::super::super::kk_blame::KkBlameStatus::new(workspace, cx));
            workspace
                .status_bar()
                .update(cx, |bar, cx| bar.add_right_item(status, window, cx));
        });
        let panel = cx
            .update(|window, cx| KkPanel::load(workspace.downgrade(), window.to_async(cx)))
            .await
            .expect("panel");
        let rows = panel.update(cx, |panel, cx| {
            panel
                .file_order
                .set(FileOrder::FileName, cx, KkPanel::show_error);
            let revision = history
                .revisions
                .iter()
                .find(|revision| revision.is_working_copy)
                .expect("working copy");
            let target = super::super::super::kk_diff::DiffTarget::change(revision);
            let repository = RepositoryView {
                stale: false,
                snapshot,
                history,
                expanded: HashMap::new(),
                comparison: None,
                located: None,
            };
            let rows = panel
                .file_order
                .files(&files)
                .iter()
                .map(|file| {
                    FileRow::new(&repository, target.clone(), (*file).clone(), "commit", true)
                })
                .collect::<Vec<_>>();
            panel.repositories = vec![repository];
            panel.visible_files = rows.clone();
            for row in rows.iter().rev() {
                panel
                    .tree_selection
                    .selected
                    .insert(TreeRowId::File(row.id.clone()));
                panel.tree_selection.anchor = Some(TreeRowId::File(row.id.clone()));
            }

            assert_eq!(
                panel
                    .selected_file_rows(&rows[0])
                    .expect("selection")
                    .iter()
                    .map(|row| row.file.path.as_str())
                    .collect::<Vec<_>>(),
                ["a.txt", "z2.txt", "z10.txt"]
            );
            let mut foreign = rows[0].clone();
            foreign.id.section = "other comparison".into();
            panel.visible_files.push(foreign.clone());
            panel
                .tree_selection
                .selected
                .insert(TreeRowId::File(foreign.id.clone()));
            assert!(panel.selected_file_rows(&rows[0]).is_err());
            panel.visible_files.pop();
            panel
                .tree_selection
                .selected
                .remove(&TreeRowId::File(foreign.id));
            rows
        });
        panel.update_in(cx, |panel, window, cx| {
            panel.run_file_batch(
                FileAction::Open(FileOpenKind::Before),
                rows.clone(),
                window,
                cx,
            );
            panel.run_file_batch(
                FileAction::Open(FileOpenKind::Before),
                rows.clone(),
                window,
                cx,
            );
            assert_eq!(panel.file_batches.len(), 1);
        });
        cx.condition(&panel, |panel, _| panel.file_batches.is_empty())
            .await;
        workspace.read_with(cx, |workspace, cx| {
            let opened = workspace
                .active_pane()
                .read(cx)
                .items()
                .filter_map(|item| item.downcast::<editor::Editor>())
                .map(|editor| {
                    let editor = editor.read(cx);
                    (
                        editor.buffer().read(cx).title(cx).into_owned(),
                        editor.buffer().read(cx).snapshot(cx).text(),
                    )
                })
                .collect::<Vec<_>>();
            assert_eq!(
                opened,
                [
                    ("a.txt".into(), "a.txt base\n".into()),
                    ("z2.txt".into(), "z2.txt base\n".into()),
                    ("z10.txt".into(), "z10.txt base\n".into())
                ]
            );
        });
        let dirty = project
            .update(cx, |project, cx| {
                project.open_local_buffer(root.join("z2.txt"), cx)
            })
            .await
            .expect("dirty buffer");
        dirty.update(cx, |buffer, cx| buffer.edit([(0..0, "draft\n")], None, cx));
        panel.update_in(cx, |panel, window, cx| {
            panel.run_file_batch(
                FileAction::Mutate(FileMutationAction::Revert),
                rows.clone(),
                window,
                cx,
            )
        });
        assert!(cx.has_pending_prompt());
        assert!(
            cx.pending_prompt()
                .expect("single confirmation")
                .0
                .contains("2 selected file(s)")
        );
        cx.simulate_prompt_answer("Cancel");
        cx.condition(&panel, |panel, _| panel.file_batches.is_empty())
            .await;
        assert_eq!(
            std::fs::read_to_string(root.join("z10.txt")).expect("cancel preserves file"),
            "z10.txt changed\n"
        );
        panel.update_in(cx, |panel, window, cx| {
            panel.run_file_batch(
                FileAction::Mutate(FileMutationAction::Revert),
                vec![
                    rows.iter()
                        .find(|row| row.file.path == "z10.txt")
                        .expect("selected file")
                        .clone(),
                ],
                window,
                cx,
            );
        });
        assert!(cx.has_pending_prompt());
        let worktree_id = project.read_with(cx, |project, cx| {
            project
                .visible_worktrees(cx)
                .next()
                .expect("worktree")
                .read(cx)
                .id()
        });
        project.update(cx, |project, cx| project.remove_worktree(worktree_id, cx));
        cx.run_until_parked();
        project
            .update(cx, |project, cx| project.create_worktree(&root, true, cx))
            .await
            .expect("reopen repository");
        cx.simulate_prompt_answer("Revert Files");
        cx.condition(&panel, |panel, _| panel.file_batches.is_empty())
            .await;
        assert_eq!(
            std::fs::read_to_string(root.join("z10.txt")).expect("closed repository retains file"),
            "z10.txt changed\n"
        );
        panel.read_with(cx, |panel, _| {
            assert!(panel.operation_error.as_ref().is_some_and(|(_, message)| {
                message.contains("0 file(s) completed; 1 failed or skipped")
                    && message.contains("repository was closed")
            }));
        });
        panel.update_in(cx, |panel, window, cx| {
            panel.run_file_batch(
                FileAction::Mutate(FileMutationAction::Revert),
                rows,
                window,
                cx,
            )
        });
        let later = project
            .update(cx, |project, cx| {
                project.open_local_buffer(root.join("a.txt"), cx)
            })
            .await
            .expect("later draft buffer");
        later.update(cx, |buffer, cx| {
            buffer.edit([(0..0, "later draft\n")], None, cx)
        });
        cx.simulate_prompt_answer("Revert Files");
        cx.condition(&panel, |panel, _| panel.file_batches.is_empty())
            .await;
        assert!(!cx.has_pending_prompt());
        assert_eq!(
            std::fs::read_to_string(root.join("z10.txt")).expect("eligible file reverted"),
            "z10.txt base\n"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).expect("later draft file retained"),
            "a.txt changed\n"
        );
        assert!(later.read_with(cx, |buffer, _| buffer.text().starts_with("later draft\n")));
        assert!(dirty.read_with(cx, |buffer, _| buffer.text().starts_with("draft\n")));
        panel.read_with(cx, |panel, _| {
            assert!(panel.operation_error.as_ref().is_some_and(|(_, message)| {
                message.contains("1 file(s) completed; 2 failed or skipped")
            }))
        });
        drop(directory);
    }
}
