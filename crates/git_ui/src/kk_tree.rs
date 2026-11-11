use super::file_actions::FileRowId;
use super::*;
use file_icons::FileIcons;
use std::{collections::HashSet, ops::Range};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) enum TreeRowId {
    Repository(PathBuf),
    Comparison(PathBuf),
    Category(PathBuf, &'static str),
    History(PathBuf),
    Revision(PathBuf, String),
    More(PathBuf),
    File(FileRowId),
}

impl TreeRowId {
    fn root(&self) -> &Path {
        match self {
            Self::Repository(root)
            | Self::Comparison(root)
            | Self::Category(root, _)
            | Self::History(root)
            | Self::Revision(root, _)
            | Self::More(root) => root,
            Self::File(file) => &file.root,
        }
    }
}

#[derive(Clone)]
enum TreeRowKind {
    Group(String),
    Recovery(String),
    Revision(Revision, kk_workbench::GraphRow),
    File(FileRow),
    More,
}

#[derive(Clone)]
pub(super) struct TreeRow {
    id: TreeRowId,
    parent: Option<TreeRowId>,
    fallback: Option<TreeRowId>,
    depth: usize,
    expanded: Option<bool>,
    kind: TreeRowKind,
    render_index: usize,
}

#[derive(Default)]
pub(super) struct TreeSelectionState {
    roots: Vec<PathBuf>,
    grouped: bool,
    pub selected: HashSet<TreeRowId>,
    pub anchor: Option<TreeRowId>,
    cursor: Option<TreeRowId>,
    expanded: HashMap<TreeRowId, bool>,
    rows: Vec<TreeRow>,
    rendered: Vec<TreeRenderRow>,
    empty_commits: HashMap<PathBuf, HashMap<String, bool>>,
    graph_widths: HashMap<PathBuf, usize>,
}

#[derive(Clone)]
enum TreeRenderRow {
    Entry(usize),
    Message {
        depth: usize,
        text: &'static str,
    },
    Error {
        root: PathBuf,
        revision: Revision,
        depth: usize,
        error: String,
    },
}

impl TreeSelectionState {
    pub(super) fn opening_rows(&self, root: &Path, path: &str) -> HashSet<TreeRowId> {
        self.rows
            .iter()
            .filter_map(|row| match &row.id {
                TreeRowId::File(file) if file.root == root && file.path == path => {
                    Some(row.id.clone())
                }
                _ => None,
            })
            .collect()
    }

    pub(super) fn update_roots(&mut self, repositories: &[RepositoryView]) {
        let roots = repositories
            .iter()
            .map(|repository| repository.snapshot.repository().root().to_owned())
            .collect::<Vec<_>>();
        if roots != self.roots {
            self.grouped = roots.len() > 1
                || (roots.len() == 1
                    && (self.grouped
                        || repositories
                            .first()
                            .is_some_and(|repository| repository.stale)));
            self.roots = roots;
        }
    }

    pub fn forget_root(&mut self, root: &Path) {
        self.empty_commits.remove(root);
        self.expanded.retain(|id, _| id.root() != root);
        self.selected.retain(|id| id.root() != root);
        if self.cursor.as_ref().is_some_and(|id| id.root() == root) {
            self.cursor = None;
        }

        if self.anchor.as_ref().is_some_and(|id| id.root() == root) {
            self.anchor = None;
        }
    }

    pub fn focused_root(&self) -> Option<&Path> {
        self.cursor.as_ref().map(TreeRowId::root)
    }

    pub fn focused_file(&self) -> Option<&FileRowId> {
        match &self.cursor {
            Some(TreeRowId::File(file)) => Some(file),
            _ => None,
        }
    }
    fn is_expanded(&self, id: &TreeRowId) -> bool {
        self.expanded.get(id).copied().unwrap_or(!matches!(
            id,
            TreeRowId::Category(_, _) | TreeRowId::Repository(_)
        ))
    }

    pub fn contains_file(&self, file: &FileRowId) -> bool {
        self.selected.contains(&TreeRowId::File(file.clone()))
    }

    fn select(&mut self, id: &TreeRowId, extend: bool, toggle: bool) {
        if extend
            && let Some(anchor) = self
                .anchor
                .as_ref()
                .and_then(|anchor| self.rows.iter().position(|row| &row.id == anchor))
            && let Some(end) = self.rows.iter().position(|row| &row.id == id)
        {
            if !toggle {
                self.selected.clear();
            }

            self.selected.extend(
                self.rows
                    .iter()
                    .skip(anchor.min(end))
                    .take(anchor.abs_diff(end) + 1)
                    .map(|row| row.id.clone()),
            );
        } else {
            if !toggle {
                self.selected.clear();
            }

            if !toggle || !self.selected.remove(id) {
                self.selected.insert(id.clone());
            }

            self.anchor = Some(id.clone());
        }

        self.cursor = Some(id.clone());
    }

    fn secondary_click(&mut self, id: &TreeRowId) {
        if !self.selected.contains(id) {
            self.selected.clear();
            self.selected.insert(id.clone());
        }

        self.cursor = Some(id.clone());
        self.anchor = Some(id.clone());
    }

    fn reconcile(&mut self, rows: Vec<TreeRow>) -> Vec<TreeRow> {
        let rows_by_id = rows
            .iter()
            .map(|row| (&row.id, row))
            .collect::<HashMap<_, _>>();
        let mut files_by_identity = HashMap::new();
        for row in &rows {
            if let TreeRowKind::File(file) = &row.kind {
                let key = (&file.id.root, &file.id.section, &file.file.path);
                files_by_identity
                    .entry(key)
                    .and_modify(|entry| *entry = None)
                    .or_insert(Some(row));
            }
        }
        for previous in &self.rows {
            if let TreeRowKind::File(file) = &previous.kind
                && file.pending
                && let Some(current) = rows_by_id.get(&previous.id).copied().or_else(|| {
                    files_by_identity
                        .get(&(&file.id.root, &file.id.section, &file.file.path))
                        .copied()
                        .flatten()
                })
            {
                if self.selected.remove(&previous.id) {
                    self.selected.insert(current.id.clone());
                }

                if self.cursor.as_ref() == Some(&previous.id) {
                    self.cursor = Some(current.id.clone());
                }

                if self.anchor.as_ref() == Some(&previous.id) {
                    self.anchor = Some(current.id.clone());
                }
            }
        }

        let previous_selection = self.selected.clone();
        let visible = rows.iter().map(|row| &row.id).collect::<HashSet<_>>();
        self.selected.retain(|id| visible.contains(id));
        if !previous_selection.is_empty() && self.selected.is_empty() {
            let removed = self
                .rows
                .iter()
                .position(|row| previous_selection.contains(&row.id));
            if let Some(index) = removed
                && let Some(removed) = self.rows.get(index)
            {
                let previous = self.rows.iter().take(index).rev().find(|row| {
                    matches!(row.kind, TreeRowKind::File(_))
                        && row.parent == removed.parent
                        && !previous_selection.contains(&row.id)
                        && visible.contains(&row.id)
                });
                let mut fallback = previous
                    .map(|row| row.id.clone())
                    .or_else(|| removed.fallback.clone().or_else(|| removed.parent.clone()));
                while let Some(id) = fallback {
                    if visible.contains(&id) {
                        self.selected.insert(id.clone());
                        self.anchor = Some(id.clone());
                        self.cursor = Some(id);
                        break;
                    }

                    fallback = self
                        .rows
                        .iter()
                        .find(|row| row.id == id)
                        .and_then(|row| row.fallback.clone().or_else(|| row.parent.clone()));
                }
            }
        }

        if self.cursor.as_ref().is_none_or(|id| !visible.contains(id)) {
            self.cursor = rows
                .iter()
                .find(|row| self.selected.contains(&row.id))
                .map(|row| row.id.clone());
        }

        if self.anchor.as_ref().is_none_or(|id| !visible.contains(id)) {
            self.anchor = self.cursor.clone();
        }

        std::mem::replace(&mut self.rows, rows)
    }
}

impl KkPanel {
    pub(super) fn update_stale(
        &mut self,
        _: &UpdateStaleWorkspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.choose_repository(window, cx, |this, index, window, cx| {
            if let Some(repository) = this.repositories.get(index) {
                let root = repository.snapshot.repository().root().to_owned();
                this.update_stale_workspace(&root, window, cx);
            }
        });
    }

    pub(super) fn update_stale_workspace(
        &mut self,
        root: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(repository) = self
            .repositories
            .iter()
            .find(|repository| repository.snapshot.repository().root() == root && repository.stale)
        else {
            return;
        };
        let snapshot = repository.snapshot.clone();
        if !self.updating_stale.insert(root.to_owned()) {
            return;
        }

        let root = root.to_owned();
        let selected = window.prompt(
            gpui::PromptLevel::Warning,
            &format!(
                "{} is stale. Run kk workspace update-stale to update this workspace?",
                root.display()
            ),
            None,
            &["Update Workspace", "Cancel"],
            cx,
        );
        let key = format!("update-stale:{}", root.display());
        let task = cx.spawn(async move |this, cx| {
            let result = async {
                if selected.await? != 0 {
                    return anyhow::Ok(false);
                }

                snapshot.repository().check_lifetime()?;
                let completed = this.update(cx, |this, cx| {
                    let snapshot = this.bind_snapshot(snapshot.clone(), cx)?;
                    anyhow::ensure!(!this.uncertain_operations.contains_key(&root), "Refresh KK Workbench to confirm the previous operation before updating the workspace");
                    let (completion, completed) = futures::channel::oneshot::channel();
                    let start = this.write_queues.entry(root.clone()).or_insert_with(|| RepositoryQueue::new(cx.background_executor().clone())).enqueue_update_stale(UpdateStaleJob { snapshot, completion });
                    this.pending_writes += 1;
                    this.pending_status = "Updating workspace";
                    this.model_generation += 1;
                    if start {
                        this.run_write_queue(root.clone(), cx);
                    }

                    cx.notify();
                    anyhow::Ok(completed)
                })??;
                completed.await?.map_err(|failure| anyhow::anyhow!(failure.message))?;
                Ok(true)
            }.await;
            this.update(cx, |this, cx| {
                this.updating_stale.remove(&root);
                this.load_tasks.remove(&format!("update-stale:{}", root.display()));
                if snapshot.repository().check_lifetime().is_ok() {
                    match result {
                        Ok(true) => {
                            this.model_generation += 1;
                            this.refresh(cx);
                        }
                        Ok(false) => {}
                        Err(error) => this.operation_error = Some((root, format!("Could not update workspace: {error:#}"))),
                    }
                }

                cx.notify();
            }).log_err();
        });
        self.load_tasks.insert(key, task);
        cx.notify();
    }

    pub(super) fn clear_commit_emptiness(&mut self, root: &Path) {
        self.tree_selection.empty_commits.remove(root);
        self.load_tasks.remove(&format!("empty:{}", root.display()));
    }

    pub(super) fn refresh_commit_emptiness(&mut self, cx: &mut Context<Self>) {
        if self.pending_writes > 0 {
            return;
        }

        for repository in &self.repositories {
            if repository.stale {
                continue;
            }

            let root = repository.snapshot.repository().root().to_owned();
            let revisions = repository
                .history
                .revisions
                .iter()
                .chain(repository.located.iter());
            let commits = revisions
                .map(|revision| revision.full_commit_id.clone())
                .collect::<HashSet<_>>();
            let cached = self
                .tree_selection
                .empty_commits
                .entry(root.clone())
                .or_default();
            cached.retain(|commit, _| commits.contains(commit));
            let mut missing = commits
                .into_iter()
                .filter(|commit| !cached.contains_key(commit))
                .collect::<Vec<_>>();
            if missing.is_empty() {
                continue;
            }

            missing.sort();
            let snapshot = repository.snapshot.clone();
            let operation = snapshot.operation().to_owned();
            let key = format!("empty:{}", root.display());
            let task_key = key.clone();
            let task = cx.spawn(async move |this, cx| {
                let result = cx
                    .background_spawn(async move { snapshot.commit_emptiness(&missing).await })
                    .await;
                this.update(cx, |this, cx| {
                    this.load_tasks.remove(&task_key);
                    if !this.repositories.iter().any(|repository| {
                        repository.snapshot.repository().root() == root
                            && repository.snapshot.operation() == operation
                            && repository.snapshot.repository().check_lifetime().is_ok()
                    }) {
                        return;
                    }

                    match result {
                        Ok(results) => {
                            this.tree_selection
                                .empty_commits
                                .entry(root)
                                .or_default()
                                .extend(results);
                            cx.notify();
                        }
                        Err(error) => this
                            .output
                            .read(cx)
                            .diagnostics()
                            .error(format!("Failed to check empty commits: {error:#}")),
                    }
                })
                .log_err();
            });
            self.load_tasks.insert(key, task);
        }
    }

    pub(super) fn collapse_tree(&mut self, cx: &mut Context<Self>) {
        for repository in &mut self.repositories {
            let root = repository.snapshot.repository().root();
            repository.expanded.clear();
            for id in [
                TreeRowId::Repository(root.to_owned()),
                TreeRowId::Comparison(root.to_owned()),
                TreeRowId::History(root.to_owned()),
            ] {
                self.tree_selection.expanded.insert(id, false);
            }
        }

        for expanded in self.tree_selection.expanded.values_mut() {
            *expanded = false;
        }

        self.load_tasks.retain(|key, _| !key.starts_with("commit:"));
        self.model_generation += 1;
        cx.notify();
    }

    pub(super) fn select_all_tree_rows(&mut self, cx: &mut Context<Self>) {
        self.tree_selection.selected = self
            .tree_selection
            .rows
            .iter()
            .map(|row| row.id.clone())
            .collect();
        if self.tree_selection.cursor.is_none() {
            self.tree_selection.cursor = self.tree_selection.rows.first().map(|row| row.id.clone());
        }

        self.tree_selection.anchor = self.tree_selection.cursor.clone();
        cx.notify();
    }

    fn tree_projection(&self) -> TreeProjection {
        TreeProjection {
            repositories: self.repositories.clone(),
            repository_paths: self.repository_paths.clone(),
            tree_selection: TreeSelectionState {
                grouped: self.tree_selection.grouped,
                expanded: self.tree_selection.expanded.clone(),
                ..TreeSelectionState::default()
            },
            list_previews: self.list_previews.clone(),
            mutation_previews: self.mutation_previews.clone(),
            file_order: self.file_order.snapshot(),
            mode: self.mode,
        }
    }

    pub(super) fn request_tree(&mut self, window: &mut Window, cx: &mut Context<Self>) -> usize {
        if self.tree_requested != Some(self.tree_revision) && self.tree_task.is_ready() {
            self.tree_selection.update_roots(&self.repositories);
            if self.reveal_selected
                && let Some((root, _, _)) = &self.selected_change
            {
                self.tree_selection
                    .expanded
                    .insert(TreeRowId::Repository(root.clone()), true);
                self.tree_selection
                    .expanded
                    .insert(TreeRowId::History(root.clone()), true);
            }
            let projection = self.tree_projection();
            let revision = self.tree_revision;
            self.tree_requested = Some(revision);
            self.tree_task = cx.spawn_in(window, async move |this, cx| {
                let rows = cx.background_spawn(async move { projection.build() }).await;
                this.update_in(cx, |this, window, cx| {
                    if this.tree_revision == revision {
                        this.apply_tree(rows, cx);
                    } else {
                        this.tree_requested = None;
                    }
                    // Publishing presentation must not invalidate its own input revision.
                    window.refresh();
                })
                .log_err();
            });
        }
        self.tree_selection.rendered.len()
    }

    fn select_tree_row(
        &mut self,
        id: &TreeRowId,
        extend: bool,
        toggle: bool,
        cx: &mut Context<Self>,
    ) {
        self.tree_selection.select(id, extend, toggle);
        if let Some(row) = self.tree_selection.rows.iter().find(|row| &row.id == id) {
            self.scroll_handle
                .scroll_to_item(row.render_index, gpui::ScrollStrategy::Nearest);
            if let TreeRowKind::Revision(revision, _) = &row.kind {
                self.selected_change = Some((
                    id.root().to_owned(),
                    revision.full_change_id.clone(),
                    revision.full_commit_id.clone(),
                ));
            }
        }

        cx.notify();
    }

    pub(super) fn selected_tree_revision(&self) -> Option<(usize, Revision)> {
        let row = self
            .tree_selection
            .rows
            .iter()
            .find(|row| Some(&row.id) == self.tree_selection.cursor.as_ref())?;
        let (index, repository) = self
            .repositories
            .iter()
            .enumerate()
            .find(|(_, repository)| repository.snapshot.repository().root() == row.id.root())?;
        let revision = match &row.kind {
            TreeRowKind::Revision(revision, _) => revision,
            TreeRowKind::File(file) => {
                let DiffScope::Change(commit) = &file.target.scope else {
                    return None;
                };
                repository
                    .history
                    .revisions
                    .iter()
                    .chain(repository.located.iter())
                    .chain(repository.comparison.iter().flat_map(|comparison| {
                        comparison.categories.iter().map(|entry| &entry.revision)
                    }))
                    .find(|revision| revision.full_commit_id == *commit)?
            }
            _ => return None,
        };
        Some((index, revision.clone()))
    }

    pub(super) fn navigate_tree(
        &mut self,
        direction: i32,
        edge: bool,
        extend: bool,
        cx: &mut Context<Self>,
    ) {
        let rows = &self.tree_selection.rows;
        if rows.is_empty() {
            return;
        }

        let current = self
            .tree_selection
            .cursor
            .as_ref()
            .and_then(|id| rows.iter().position(|row| &row.id == id));
        let index = if edge {
            if direction < 0 { 0 } else { rows.len() - 1 }
        } else if let Some(current) = current {
            current
                .saturating_add_signed(direction as isize)
                .min(rows.len() - 1)
        } else {
            0
        };
        if let Some(row) = rows.get(index) {
            let id = row.id.clone();
            self.select_tree_row(&id, extend, false, cx);
        }
    }

    pub(super) fn expand_tree(
        &mut self,
        expand: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(row) = self
            .tree_selection
            .cursor
            .as_ref()
            .and_then(|id| self.tree_selection.rows.iter().find(|row| &row.id == id))
            .cloned()
        else {
            return;
        };
        if row.expanded == Some(!expand) {
            self.toggle_tree_row(&row, cx);
        } else if expand && row.expanded == Some(true) {
            if let Some(child) = self
                .tree_selection
                .rows
                .iter()
                .find(|child| child.parent.as_ref() == Some(&row.id))
            {
                let id = child.id.clone();
                self.select_tree_row(&id, false, false, cx);
            }
        } else if !expand && let Some(parent) = row.parent {
            self.select_tree_row(&parent, false, false, cx);
        }

        window.focus(&self.focus, cx);
    }

    fn toggle_tree_row(&mut self, row: &TreeRow, cx: &mut Context<Self>) {
        let Some(index) = self
            .repositories
            .iter()
            .position(|repository| repository.snapshot.repository().root() == row.id.root())
        else {
            return;
        };
        match &row.kind {
            TreeRowKind::Revision(revision, _) => {
                self.load_files(index, revision.clone(), false, cx)
            }
            TreeRowKind::Group(_) => {
                self.tree_selection
                    .expanded
                    .insert(row.id.clone(), row.expanded != Some(true));
                cx.notify();
            }
            TreeRowKind::More => {
                self.load_more_commits(index, cx);
            }
            TreeRowKind::File(_) | TreeRowKind::Recovery(_) => {}
        }
    }

    pub(super) fn confirm_tree(
        &mut self,
        mode: DiffOpenMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(row) = self
            .tree_selection
            .cursor
            .as_ref()
            .and_then(|id| self.tree_selection.rows.iter().find(|row| &row.id == id))
            .cloned()
        else {
            return;
        };
        if matches!(row.kind, TreeRowKind::Recovery(_)) {
            self.update_stale_workspace(row.id.root(), window, cx);
        } else if let TreeRowKind::File(file) = row.kind {
            if file.pending {
                self.run_file_action(FileAction::Open(FileOpenKind::Current), file, window, cx);
                return;
            }

            if let Some(index) = self
                .repositories
                .iter()
                .position(|repository| repository.snapshot.repository().root() == file.id.root)
            {
                self.open_file(
                    index,
                    file.target,
                    file.file,
                    file.working_copy,
                    mode,
                    window,
                    cx,
                );
            }
        } else {
            self.toggle_tree_row(&row, cx);
        }
    }

    #[cfg(test)]
    pub(super) fn update_tree(&mut self, cx: &mut Context<Self>) -> usize {
        self.tree_selection.update_roots(&self.repositories);
        if self.reveal_selected
            && let Some((root, _, _)) = &self.selected_change
        {
            self.tree_selection
                .expanded
                .insert(TreeRowId::Repository(root.clone()), true);
            self.tree_selection
                .expanded
                .insert(TreeRowId::History(root.clone()), true);
        }

        let presentation = self.tree_projection().build();
        self.apply_tree(presentation, cx)
    }

    fn apply_tree(&mut self, presentation: TreePresentation, cx: &mut Context<Self>) -> usize {
        let TreePresentation {
            tree_rows,
            rendered,
            visible_files,
            graph_widths,
        } = presentation;
        let previous_rows = self.tree_selection.reconcile(tree_rows);
        let previous_rendered = std::mem::replace(&mut self.tree_selection.rendered, rendered);
        let previous_files = std::mem::replace(&mut self.visible_files, visible_files);
        let previous_widths =
            std::mem::replace(&mut self.tree_selection.graph_widths, graph_widths);
        cx.background_spawn(async move {
            drop((
                previous_rows,
                previous_rendered,
                previous_files,
                previous_widths,
            ));
        })
        .detach();
        if self.reveal_selected && !self.loading {
            let id = self.selected_change.as_ref().and_then(|(root, change, commit)| {
                let candidates = self.tree_selection.rows.iter().filter(|row| {
                    row.id.root() == root
                        && matches!(&row.kind, TreeRowKind::Revision(revision, _) if revision.full_change_id == *change)
                }).collect::<Vec<_>>();
                candidates.iter().find(|row| matches!(&row.kind, TreeRowKind::Revision(revision, _) if revision.full_commit_id == *commit))
                    .or_else(|| (candidates.len() == 1).then(|| candidates.first()).flatten())
                    .map(|row| row.id.clone())
            });
            if let Some(id) = id {
                self.select_tree_row(&id, false, false, cx);
                self.reveal_selected = false;
            }
        }

        self.tree_selection.rendered.len()
    }

    pub(super) fn render_tree_rows(
        &self,
        range: Range<usize>,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        range
            .filter_map(|index| {
                let element = match self.tree_selection.rendered.get(index)? {
                    TreeRenderRow::Entry(row) => self
                        .tree_selection
                        .rows
                        .get(*row)
                        .map(|row| self.render_tree_row(row.clone(), cx)),
                    TreeRenderRow::Message { depth, text } => Some(
                        ListItem::new(("message", index))
                            .height(rems(1.75))
                            .indent_level(*depth + 1)
                            .child(Label::new(*text).color(Color::Muted))
                            .into_any_element(),
                    ),
                    TreeRenderRow::Error {
                        root,
                        revision,
                        depth,
                        error,
                    } => {
                        let menu_root = root.clone();
                        let menu_revision = revision.clone();
                        Some(
                            ListItem::new(("error", index))
                                .height(rems(1.75))
                                .indent_level(*depth + 1)
                                .child(Label::new(error.clone()).color(Color::Error).truncate())
                                .end_slot(
                                    IconButton::new(("show-output", index), IconName::Terminal)
                                        .aria_label("Show Output")
                                        .when(self.context_menu.is_none(), |item| {
                                            item.tooltip(Tooltip::text("Show Output"))
                                        })
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.show_output(window, cx)
                                        })),
                                )
                                .show_end_slot_on_hover()
                                .on_secondary_mouse_down(cx.listener(
                                    move |this, event: &gpui::MouseDownEvent, window, cx| {
                                        cx.stop_propagation();
                                        if let Some(index) =
                                            this.repositories.iter().position(|repository| {
                                                repository.snapshot.repository().root() == menu_root
                                            })
                                        {
                                            window.focus(&this.focus, cx);
                                            this.revision_menu(
                                                index,
                                                menu_revision.clone(),
                                                event.position,
                                                window,
                                                cx,
                                            );
                                        }
                                    },
                                ))
                                .into_any_element(),
                        )
                    }
                }?;
                Some(self.with_history_graph(index, element, cx))
            })
            .collect()
    }

    fn with_history_graph(
        &self,
        index: usize,
        element: gpui::AnyElement,
        cx: &App,
    ) -> gpui::AnyElement {
        if self.mode != HistoryMode::Log {
            return element;
        }
        let entry = self.tree_selection.rendered.get(index);
        let row_at = |rendered: &TreeRenderRow| match rendered {
            TreeRenderRow::Entry(index) => self.tree_selection.rows.get(*index),
            _ => None,
        };
        let current = entry.and_then(row_at);
        let previous = self
            .tree_selection
            .rendered
            .get(..=index)
            .into_iter()
            .flatten()
            .rev()
            .filter_map(row_at)
            .find(|row| matches!(row.kind, TreeRowKind::Revision(_, _)));
        let Some(previous) = previous else {
            return element;
        };
        let TreeRowKind::Revision(revision, graph) = &previous.kind else {
            return element;
        };
        let is_node = current.is_some_and(|row| row.id == previous.id);
        let in_history = is_node
            || match entry {
                Some(TreeRenderRow::Entry(_)) => current.is_some_and(|row| {
                    row.id.root() == previous.id.root()
                        && (matches!(row.kind, TreeRowKind::More)
                            || row.parent.as_ref() == Some(&previous.id))
                }),
                Some(TreeRenderRow::Message { .. }) => true,
                Some(TreeRenderRow::Error { root, .. }) => root == previous.id.root(),
                None => false,
            };
        if !in_history {
            return element;
        }
        let width = self
            .tree_selection
            .graph_widths
            .get(previous.id.root())
            .copied()
            .unwrap_or(1);
        let graph = graph.clone();
        let conflict = revision.has_conflict;
        let working = revision.is_working_copy;
        let colors = cx.theme().accents().clone();
        let node_color = if conflict {
            Color::Error
        } else {
            Color::Accent
        }
        .color(cx);
        h_flex()
            .w_full()
            .child(
                div()
                    .id("kk-history-graph")
                    .debug_selector(|| "kk-history-graph".to_owned())
                    .child(
                        gpui::canvas(
                            |_, _, _| {},
                            move |bounds, _, window, _| {
                                let center_y = bounds.origin.y + bounds.size.height / 2.;
                                let x =
                                    |column: usize| bounds.origin.x + px(8. + column as f32 * 12.);
                                let mut draw = |from, to, column| {
                                    let mut path = gpui::PathBuilder::stroke(px(1.5));
                                    path.move_to(from);
                                    path.line_to(to);
                                    match path.build() {
                                        Ok(path) => window.paint_path(
                                            path,
                                            colors.color_for_index(column as u32),
                                        ),
                                        Err(error) => {
                                            log::error!("Could not draw KK history edge: {error}")
                                        }
                                    }
                                };
                                if is_node {
                                    for column in &graph.incoming {
                                        draw(
                                            gpui::point(x(*column), bounds.origin.y),
                                            gpui::point(
                                                x(*column),
                                                if *column == graph.column {
                                                    center_y
                                                } else {
                                                    bounds.bottom()
                                                },
                                            ),
                                            *column,
                                        );
                                    }
                                    for column in &graph.outgoing {
                                        if graph.incoming.contains(column)
                                            || *column != graph.column
                                        {
                                            continue;
                                        }
                                        draw(
                                            gpui::point(x(*column), center_y),
                                            gpui::point(x(*column), bounds.bottom()),
                                            *column,
                                        );
                                    }
                                    for column in &graph.parents {
                                        draw(
                                            gpui::point(x(graph.column), center_y),
                                            gpui::point(
                                                x(*column),
                                                center_y + bounds.size.height / 2.,
                                            ),
                                            graph.column,
                                        );
                                    }
                                } else {
                                    for column in &graph.outgoing {
                                        draw(
                                            gpui::point(x(*column), bounds.origin.y),
                                            gpui::point(x(*column), bounds.bottom()),
                                            *column,
                                        );
                                    }
                                }
                                if is_node && graph.omitted_parents > 0 {
                                    for offset in [3., 8.] {
                                        draw(
                                            gpui::point(x(graph.column), center_y + px(offset)),
                                            gpui::point(
                                                x(graph.column),
                                                center_y + px(offset + 2.),
                                            ),
                                            graph.column,
                                        );
                                    }
                                }
                                if is_node {
                                    let radius = if working { px(4.) } else { px(3.) };
                                    window.paint_quad(
                                        gpui::fill(
                                            gpui::Bounds::new(
                                                gpui::point(
                                                    x(graph.column) - radius,
                                                    center_y - radius,
                                                ),
                                                gpui::size(radius * 2., radius * 2.),
                                            ),
                                            node_color,
                                        )
                                        .corner_radii(radius),
                                    );
                                }
                            },
                        )
                        .w(px(16. + width as f32 * 12.))
                        .h(rems(1.75))
                        .flex_shrink_0(),
                    )
                    .flex_shrink_0(),
            )
            .child(div().flex_1().min_w_0().child(element))
            .into_any_element()
    }

    fn render_tree_row(&self, row: TreeRow, cx: &mut Context<Self>) -> gpui::AnyElement {
        let mut tooltip = None;
        let mut revision_tooltip = None;
        let mut directory = None;
        let mut source = None;
        let mut icon = None;
        let mut badge = None;
        let label = match &row.kind {
            TreeRowKind::Group(label) => {
                if matches!(row.id, TreeRowId::Repository(_)) {
                    tooltip = Some(row.id.root().display().to_string());
                    icon = Some(IconName::GitWorktree);
                }

                label.clone()
            }
            TreeRowKind::Recovery(label) => {
                icon = Some(IconName::Warning);
                tooltip = Some(row.id.root().display().to_string());
                format!(
                    "{label} · {}",
                    if self.updating_stale.contains(row.id.root()) {
                        "Updating workspace…"
                    } else {
                        "Workspace is stale — select to update"
                    }
                )
            }
            TreeRowKind::Revision(revision, graph) => {
                let empty = self
                    .tree_selection
                    .empty_commits
                    .get(row.id.root())
                    .and_then(|commits| commits.get(&revision.full_commit_id))
                    == Some(&true);
                let references = revision
                    .bookmarks
                    .iter()
                    .cloned()
                    .chain(revision.tags.iter().cloned())
                    .chain(revision.remote_bookmarks())
                    .collect::<Vec<_>>();
                let mut details = vec![
                    (
                        "Change ID".to_owned(),
                        revision.full_change_id.clone(),
                        Color::Accent,
                    ),
                    (
                        "Commit ID".to_owned(),
                        revision.full_commit_id.clone(),
                        Color::Accent,
                    ),
                    (
                        String::new(),
                        revision.full_description.clone(),
                        Color::Default,
                    ),
                    (
                        "Author".to_owned(),
                        format!("{} <{}>", revision.author_name, revision.author_email),
                        Color::Info,
                    ),
                    (
                        "Committer".to_owned(),
                        format!("{} <{}>", revision.committer_name, revision.committer_email),
                        Color::Info,
                    ),
                ];
                if !references.is_empty() {
                    details.push((
                        "References".to_owned(),
                        references.join(", "),
                        Color::Accent,
                    ));
                }
                for (visible, text, color) in [
                    (
                        revision.has_conflict,
                        "Unresolved conflicts in this change".to_owned(),
                        Color::Error,
                    ),
                    (
                        empty,
                        "No changes relative to the merged parents".to_owned(),
                        Color::Success,
                    ),
                    (
                        revision.parent_commit_ids.len() > 1,
                        format!(
                            "Merge change · {} parents",
                            revision.parent_commit_ids.len()
                        ),
                        Color::Muted,
                    ),
                    (
                        revision.has_multiple_children,
                        "Fork point · multiple children".to_owned(),
                        Color::Muted,
                    ),
                    (
                        graph.omitted_parents > 0,
                        format!(
                            "{} parent paths continue outside the loaded history",
                            graph.omitted_parents
                        ),
                        Color::Muted,
                    ),
                    (revision.is_root, "Root change".to_owned(), Color::Muted),
                    (
                        revision.is_working_copy,
                        "Working copy (@)".to_owned(),
                        Color::Accent,
                    ),
                ] {
                    if visible {
                        details.push((String::new(), text, color));
                    }
                }
                revision_tooltip = Some(details);
                icon = Some(if !references.is_empty() {
                    IconName::Bookmark
                } else if revision.has_multiple_children {
                    IconName::GitBranch
                } else if revision.parent_commit_ids.len() > 1 {
                    IconName::GitGraph
                } else {
                    IconName::GitCommit
                });
                revision.subject().to_owned()
            }
            TreeRowKind::File(file) => {
                let (status, color) = match file.file.change_type {
                    kk_workbench::ChangeKind::Modified => ("M", Color::VersionControlModified),
                    kk_workbench::ChangeKind::Added => ("A", Color::VersionControlAdded),
                    kk_workbench::ChangeKind::Deleted => ("D", Color::VersionControlDeleted),
                    kk_workbench::ChangeKind::Renamed => ("R", Color::VersionControlModified),
                    kk_workbench::ChangeKind::Copied => ("C", Color::VersionControlAdded),
                };
                badge = Some((status, color));
                let mut details = match &file.file.renamed_from {
                    Some(previous) => format!(
                        "{previous} → {}\n{}",
                        file.file.path,
                        file.id.root.display()
                    ),
                    None => file.id.root.join(&file.file.path).display().to_string(),
                };
                if file.id.section.starts_with("category:") {
                    source = Some(file.target.label.clone());
                    details.push_str(&format!("\nChange: {}", file.target.identity));
                }

                if file.pending {
                    if let Some(action) = file.pending_action {
                        source = Some(format!("{action}…"));
                        details.push_str("\nOperation is awaiting confirmation");
                    } else {
                        source = Some("Saving…".into());
                        details.push_str("\nClassification is awaiting confirmation");
                    }
                }

                tooltip = Some(details);
                if file.id.section.starts_with("category:") {
                    let mut fields = vec![(
                        "File".to_owned(),
                        file.id.root.join(&file.file.path).display().to_string(),
                        Color::Default,
                    )];
                    if let Some(previous) = &file.file.renamed_from {
                        fields.push(("Renamed from".to_owned(), previous.clone(), Color::Muted));
                    }
                    if let Some(revision) = &file.target.revision {
                        fields.extend([
                            (
                                "Change ID".to_owned(),
                                revision.full_change_id.clone(),
                                Color::Accent,
                            ),
                            (
                                "Commit ID".to_owned(),
                                revision.full_commit_id.clone(),
                                Color::Accent,
                            ),
                            (
                                "Author".to_owned(),
                                format!("{} <{}>", revision.author_name, revision.author_email),
                                Color::Info,
                            ),
                            (
                                String::new(),
                                revision.full_description.clone(),
                                Color::Default,
                            ),
                        ]);
                    } else {
                        fields.push((
                            "Source".to_owned(),
                            file.target.label.clone(),
                            Color::Accent,
                        ));
                    }
                    if file.pending {
                        fields.push((
                            "Status".to_owned(),
                            source.clone().unwrap_or_default(),
                            Color::Muted,
                        ));
                    }
                    revision_tooltip = Some(fields);
                    tooltip = None;
                }
                if let Some((parent, name)) = file.file.path.rsplit_once('/') {
                    directory = Some(parent.to_owned());
                    name.to_owned()
                } else {
                    file.file.path.clone()
                }
            }
            TreeRowKind::More => match self.history_loads.get(row.id.root()) {
                Some(HistoryLoad::Loading) => "Loading more commits…",
                Some(HistoryLoad::Failed) => "Retry Loading More Commits",
                None => "Load More Commits",
            }
            .into(),
        };
        let conflict = match &row.kind {
            TreeRowKind::File(file) => file.file.has_conflict,
            TreeRowKind::Revision(revision, _) => revision.has_conflict,
            _ => false,
        };
        let warning = matches!(row.kind, TreeRowKind::Recovery(_));
        let disclosure_row = row.clone();
        let click_row = row.clone();
        let menu_row = row.clone();
        let root = row.id.root().to_owned();
        let mut buttons = h_flex();
        let opening = self
            .opening_feedback
            .as_ref()
            .is_some_and(|(_, _, rows)| rows.contains(&row.id));
        if opening {
            buttons = buttons.child(ui::SpinnerLabel::new());
        }
        if matches!(row.id, TreeRowId::Comparison(_)) {
            let open_root = root.clone();
            buttons =
                buttons.child(
                    IconButton::new(
                        SharedString::from(format!("open-changes-{:?}", row.id)),
                        IconName::Diff,
                    )
                    .aria_label("Open Changes")
                    .when(self.context_menu.is_none(), |item| {
                        item.tooltip(Tooltip::text("Open Changes"))
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        if let Some(index) = this.repositories.iter().position(|repository| {
                            repository.snapshot.repository().root() == open_root
                        }) {
                            this.open_all_changes(index, None, window, cx);
                        }
                    })),
                );
        }
        match &row.kind {
            TreeRowKind::Recovery(_) => {}
            TreeRowKind::File(file) => {
                let mut actions = Vec::new();
                if !file.pending && file.working_copy {
                    actions.push((
                        "Revert File",
                        IconName::Undo,
                        FileAction::Mutate(FileMutationAction::Revert),
                    ));
                }
                if !file.pending && file.can_move_child {
                    actions.push((
                        "Move File Changes to Child",
                        IconName::ArrowUp,
                        FileAction::Mutate(FileMutationAction::Move(FileMoveDirection::Child)),
                    ));
                }
                if !file.pending && file.can_move_parent {
                    actions.push((
                        "Move File Changes to Parent",
                        IconName::ArrowDown,
                        FileAction::Mutate(FileMutationAction::Move(FileMoveDirection::Parent)),
                    ));
                }
                actions.push((
                    "Open File",
                    IconName::File,
                    FileAction::Open(FileOpenKind::Current),
                ));
                for (label, icon, action) in actions {
                    let file = file.clone();
                    buttons = buttons.child(
                        IconButton::new(SharedString::from(format!("{label}-{:?}", row.id)), icon)
                            .aria_label(label)
                            .when(self.context_menu.is_none(), |item| {
                                item.tooltip(Tooltip::text(label))
                            })
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.run_file_action(action, file.clone(), window, cx);
                            })),
                    );
                }
            }
            TreeRowKind::Revision(revision, _) => {
                let searched = revision.clone();
                buttons = buttons.child(
                    IconButton::new(
                        SharedString::from(format!("search-{:?}", row.id)),
                        IconName::MagnifyingGlass,
                    )
                    .aria_label("Search in Revision")
                    .when(self.context_menu.is_none(), |item| {
                        item.tooltip(Tooltip::text("Search in Revision"))
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        if let Some(index) = this
                            .repositories
                            .iter()
                            .position(|repository| repository.snapshot.repository().root() == root)
                        {
                            this.search_revision(index, Some(searched.clone()), window, cx);
                        }
                    })),
                );
            }
            _ if matches!(row.id, TreeRowId::Comparison(_)) => {
                let close_root = root.clone();
                buttons = buttons
                    .child(
                        IconButton::new(
                            SharedString::from(format!("close-{:?}", row.id)),
                            IconName::Close,
                        )
                        .aria_label("Close Comparison")
                        .when(self.context_menu.is_none(), |item| {
                            item.tooltip(Tooltip::text("Close Comparison"))
                        })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(index) = this.repositories.iter().position(|repository| {
                                repository.snapshot.repository().root() == close_root
                            }) {
                                this.close_repository_comparison(index, cx);
                            }
                        })),
                    )
                    .child(
                        IconButton::new(
                            SharedString::from(format!("search-{:?}", row.id)),
                            IconName::MagnifyingGlass,
                        )
                        .aria_label("Search in Changes")
                        .when(self.context_menu.is_none(), |item| {
                            item.tooltip(Tooltip::text("Search in Changes"))
                        })
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                cx.stop_propagation();
                                if let Some(index) =
                                    this.repositories.iter().position(|repository| {
                                        repository.snapshot.repository().root() == root
                                    })
                                {
                                    this.search_revision(index, None, window, cx);
                                }
                            },
                        )),
                    );
            }
            _ => {}
        }

        let row_content = h_flex()
            .overflow_hidden()
            .flex_1()
            .min_w_0()
            .gap_1p5()
            .child({
                let content = div().min_w_0().flex_shrink_1();
                if let TreeRowKind::Revision(revision, graph) = &row.kind {
                    let references = revision
                        .bookmarks
                        .iter()
                        .cloned()
                        .chain(revision.tags.iter().cloned())
                        .chain(revision.remote_bookmarks())
                        .collect::<Vec<_>>();
                    content.child(
                        h_flex()
                            .gap_1()
                            .min_w_0()
                            .when(revision.is_working_copy, |view| {
                                view.font_weight(gpui::FontWeight::BOLD)
                            })
                            .when(self.mode == HistoryMode::Lineage, |view| {
                                view.child(Label::new(graph.prefix.clone()).color(Color::Accent))
                            })
                            .child(crate::commit_metadata::change_identifier(
                                revision.change_id.clone(),
                                &revision.change_id_prefix,
                            ))
                            .child(
                                Label::new(if revision.author_name.is_empty() {
                                    revision
                                        .author_email
                                        .split('@')
                                        .next()
                                        .unwrap_or_default()
                                        .to_owned()
                                } else {
                                    revision.author_name.clone()
                                })
                                .color(Color::Info),
                            )
                            .when(revision.has_conflict, |view| {
                                view.child(Label::new("(conflict)").color(Color::Error))
                            })
                            .when(
                                self.tree_selection
                                    .empty_commits
                                    .get(row.id.root())
                                    .and_then(|commits| commits.get(&revision.full_commit_id))
                                    == Some(&true),
                                |view| view.child(Label::new("(empty)").color(Color::Success)),
                            )
                            .child(
                                div().min_w_0().flex_shrink_1().child(
                                    Label::new(revision.subject().to_owned())
                                        .color(if conflict {
                                            Color::Error
                                        } else {
                                            Color::Default
                                        })
                                        .truncate(),
                                ),
                            )
                            .when_some(references.first(), |view, reference| {
                                view.child(Label::new("·").color(Color::Muted))
                                    .child(Label::new(reference.clone()).color(Color::Accent))
                            })
                            .when(references.len() > 1, |view| {
                                view.child(
                                    Label::new(format!("+{}", references.len() - 1))
                                        .color(Color::Muted),
                                )
                            }),
                    )
                } else {
                    content.child(Label::new(label).truncate().color(if conflict {
                        Color::Error
                    } else if warning {
                        Color::Warning
                    } else {
                        badge.map_or(Color::Default, |(_, color)| color)
                    }))
                }
            })
            .when_some(source, |item, source| {
                item.child(Label::new(source).color(
                    if matches!(&row.kind, TreeRowKind::File(file) if !file.pending) {
                        Color::Accent
                    } else {
                        Color::Muted
                    },
                ))
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .when_some(directory, |element, directory| {
                        element.child(Label::new(directory).color(Color::Muted).truncate())
                    }),
            );

        ListItem::new(SharedString::from(format!("kk-tree-{:?}", row.id)))
            .disabled(
                matches!(row.kind, TreeRowKind::More)
                    && self.history_loads.get(row.id.root()) == Some(&HistoryLoad::Loading)
                    || matches!(row.kind, TreeRowKind::Recovery(_))
                        && self.updating_stale.contains(row.id.root()),
            )
            .height(rems(1.75))
            .indent_level(row.depth + 1)
            .when_some(tooltip, |item, tooltip| {
                item.when(self.context_menu.is_none(), |item| {
                    item.tooltip(Tooltip::text(tooltip))
                })
            })
            .when_some(revision_tooltip, |item, details| {
                item.when(self.context_menu.is_none(), |item| {
                    item.tooltip(super::super::commit_metadata::tooltip(details))
                })
            })
            .when_some(icon, |item, icon| {
                item.start_slot(Icon::new(icon).color(if conflict {
                    Color::Error
                } else if warning {
                    Color::Warning
                } else {
                    Color::Muted
                }))
            })
            .when(matches!(row.kind, TreeRowKind::File(_)), |item| {
                if let TreeRowKind::File(file) = &row.kind {
                    item.start_slot(
                        FileIcons::get_icon(Path::new(&file.file.path), cx)
                            .map(Icon::from_path)
                            .unwrap_or_else(|| Icon::new(IconName::File))
                            .color(if conflict { Color::Error } else { Color::Muted }),
                    )
                } else {
                    item
                }
            })
            .toggle_state(self.tree_selection.selected.contains(&row.id))
            .when_some(row.expanded, |item, expanded| {
                item.toggle(expanded)
                    .always_show_disclosure_icon(true)
                    .on_toggle(cx.listener(move |this, _, window, cx| {
                        this.toggle_tree_row(&disclosure_row, cx);
                        window.focus(&this.focus, cx);
                    }))
            })
            .map(|item| {
                if matches!(row.kind, TreeRowKind::File(_)) {
                    item.child(file_row_content(
                        row_content,
                        buttons,
                        self.tree_selection.selected.contains(&row.id),
                        opening,
                        cx,
                    ))
                } else {
                    item.child(row_content).end_slot(buttons).when(
                        !opening && !self.tree_selection.selected.contains(&row.id),
                        |item| item.show_end_slot_on_hover(),
                    )
                }
            })
            .when(conflict, |item| {
                item.child(
                    div()
                        .flex_shrink_0()
                        .child(Icon::new(IconName::Warning).color(Color::Error)),
                )
            })
            .when_some(badge, |item, (badge, color)| {
                item.child(div().flex_shrink_0().child(Label::new(badge).color(color)))
            })
            .prioritize_end_slot()
            .on_click(
                cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                    let modifiers = event.modifiers();
                    let toggle = if cx.platform_info().os == gpui::OperatingSystem::Mac {
                        modifiers.platform
                    } else {
                        modifiers.control
                    };
                    this.select_tree_row(&click_row.id, modifiers.shift, toggle, cx);
                    window.focus(&this.focus, cx);
                    if !modifiers.shift && !toggle {
                        let mode = if event.click_count() == 1 {
                            DiffOpenMode::Preview
                        } else {
                            DiffOpenMode::Open
                        };
                        this.confirm_tree(mode, window, cx);
                    }
                }),
            )
            .on_secondary_mouse_down(cx.listener(
                move |this, event: &gpui::MouseDownEvent, window, cx| {
                    this.tree_selection.secondary_click(&menu_row.id);
                    window.focus(&this.focus, cx);
                    cx.notify();
                    match &menu_row.kind {
                        TreeRowKind::File(file) => {
                            this.show_file_actions(file.clone(), event.position, window, cx)
                        }
                        TreeRowKind::Revision(revision, _) => {
                            if let Some(index) = this.repositories.iter().position(|repository| {
                                repository.snapshot.repository().root() == menu_row.id.root()
                            }) {
                                this.revision_menu(
                                    index,
                                    revision.clone(),
                                    event.position,
                                    window,
                                    cx,
                                );
                            }
                        }
                        _ => {}
                    }
                },
            ))
            .into_any_element()
    }
}

fn file_row_content(
    content: gpui::Div,
    buttons: gpui::Div,
    selected: bool,
    opening: bool,
    cx: &App,
) -> gpui::Div {
    let colors = cx.theme().colors();
    content.relative().child(
        buttons
            .absolute()
            .right_0()
            .bg(colors.panel_background.blend(colors.ghost_element_selected))
            .when(!selected, |buttons| {
                buttons
                    .bg(colors.panel_background.blend(colors.ghost_element_hover))
                    .when(!opening, |buttons| buttons.visible_on_hover("list_item"))
            }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Modifiers, TestAppContext, VisualTestContext};
    use project::{FakeFs, Project};
    use workspace::MultiWorkspace;

    struct FileActionLayout {
        width: f32,
        selected: bool,
    }

    impl Render for FileActionLayout {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div().w(px(self.width)).child(
                ListItem::new("file-layout")
                    .indent_level(4)
                    .child(file_row_content(
                        h_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_1p5()
                            .child(
                                div().min_w_0().flex_shrink_1().child(
                                    Label::new("a_very_long_file_name_that_must_truncate.rs")
                                        .truncate(),
                                ),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .debug_selector(|| "file-path".to_owned())
                                    .child(
                                        Label::new("deeply/nested/directory/with/a/long/path")
                                            .truncate(),
                                    ),
                            ),
                        h_flex()
                            .child(IconButton::new("revert", IconName::Undo))
                            .child(IconButton::new("move", IconName::ArrowDown))
                            .child(IconButton::new("open", IconName::File)),
                        self.selected,
                        false,
                        cx,
                    ))
                    .child(
                        div()
                            .flex_shrink_0()
                            .debug_selector(|| "file-status".to_owned())
                            .child(Label::new("M")),
                    )
                    .prioritize_end_slot(),
            )
        }
    }

    #[gpui::test]
    fn kk_file_actions_remain_inside_narrow_rows(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings = SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let (view, cx) = cx.add_window_view(|_, _| FileActionLayout {
            width: 240.,
            selected: true,
        });
        for width in [240., 360.] {
            for selected in [true, false] {
                view.update(cx, |view, cx| {
                    view.width = width;
                    view.selected = selected;
                    cx.notify();
                });
                cx.simulate_mouse_move(
                    gpui::point(px(width + 20.), px(60.)),
                    None,
                    Modifiers::none(),
                );
                cx.update(|window, cx| window.draw(cx).clear(cx));
                let path_before = cx.debug_bounds("file-path").expect("path without hover");
                let status_before = cx
                    .debug_bounds("file-status")
                    .expect("status without hover");
                cx.simulate_mouse_move(
                    gpui::point(px(width - 10.), px(12.)),
                    None,
                    Modifiers::none(),
                );
                cx.update(|window, cx| window.draw(cx).clear(cx));
                assert_eq!(cx.debug_bounds("file-path"), Some(path_before));
                assert_eq!(cx.debug_bounds("file-status"), Some(status_before));
                let mut previous_right = px(0.);
                for selector in ["ICON-Undo", "ICON-ArrowDown", "ICON-File"] {
                    let bounds = cx.debug_bounds(selector).expect("visible action");
                    assert!(bounds.size.width > px(0.));
                    assert!(bounds.left() >= previous_right);
                    assert!(bounds.right() <= px(width), "{selector}: {bounds:?}");
                    previous_right = bounds.right();
                }
                assert!(previous_right <= status_before.left());
                assert_eq!(path_before.right(), previous_right);
            }
        }
    }

    async fn category_repository() -> anyhow::Result<(tempfile::TempDir, RepositoryView)> {
        use crate::kk_panel::remote_tests::run_kk;

        let directory = tempfile::tempdir()?;
        run_kk(directory.path(), &["init", "."]).await?;
        for path in ["a.txt", "b.txt", "c.txt"] {
            std::fs::write(directory.path().join(path), "base\n")?;
        }

        let repository = kk_workbench::Repository::open(directory.path(), Path::new("kk")).await?;
        let base = repository.snapshot(true).await?.exact_revision("@").await?;
        run_kk(directory.path(), &["new", "-m", "KKW: REVIEWED"]).await?;
        std::fs::write(directory.path().join("a.txt"), "first category\n")?;
        run_kk(directory.path(), &["new", "-m", "KKW: REVIEWED"]).await?;
        std::fs::write(directory.path().join("b.txt"), "second category\n")?;
        run_kk(directory.path(), &["new", "-m", "work"]).await?;
        std::fs::write(directory.path().join("c.txt"), "current work\n")?;
        let snapshot = repository.snapshot(true).await?;
        let history = snapshot.history(HistoryMode::Lineage, 50).await?;
        let comparison = Comparison::load(
            &snapshot,
            ComparisonIntent::for_target(base.revset(), "base".into(), None),
        )
        .await?;
        Ok((
            directory,
            RepositoryView {
                stale: false,
                snapshot,
                history,
                comparison: Some(comparison),
                expanded: HashMap::new(),
                located: None,
            },
        ))
    }

    #[gpui::test]
    async fn category_only_conflicts_preserve_file_metadata_and_project_resolution(
        cx: &mut TestAppContext,
    ) {
        cx.background_executor.allow_parking();
        let (_directory, mut repository) = cx
            .background_spawn(async { futures_lite::future::block_on(category_repository()) })
            .await
            .expect("category repository");
        repository.history.revisions.clear();
        let entry = repository
            .comparison
            .as_mut()
            .expect("comparison")
            .categories
            .first_mut()
            .expect("category");
        let file = entry.files.first_mut().expect("category file");
        file.change_type = kk_workbench::ChangeKind::Renamed;
        file.renamed_from = Some("previous.txt".into());
        file.has_conflict = true;
        let expected = file.clone();
        let revision = entry.revision.clone();
        let mut previews = MutationPreviews::default();
        let mut request = ConflictRequest {
            snapshot: repository.snapshot.clone(),
            conflict: kk_workbench::FileConflict {
                repository_root: repository.snapshot.repository().root().to_owned(),
                path: expected.path.clone(),
                change_id: revision.full_change_id,
                commit_id: revision.full_commit_id,
                immutable: false,
                inputs: Vec::new(),
                materialized: Vec::new(),
                marker_length: 7,
                binary: false,
            },
            result: Some(b"resolved\n".to_vec()),
            executable: None,
        };
        let resolved = previews.conflict(&repository, &request);
        let rows = ListPreviews::default().files_with_mutations(&repository, &previews);
        let row = rows
            .iter()
            .find(|row| row.file.path == expected.path)
            .expect("resolved row");
        assert!(!row.file.has_conflict);
        assert!(row.pending);
        assert_eq!(row.pending_action, Some("Applying resolution"));
        assert_eq!(row.file.change_type, expected.change_type);
        assert_eq!(row.file.renamed_from, expected.renamed_from);
        let dependency = row
            .classification_dependency
            .clone()
            .expect("write dependency");
        let mut classifications = ListPreviews::default();
        let next = classifications.push_from(
            repository.snapshot.clone(),
            &repository.comparison.as_ref().expect("comparison").intent,
            row.target.clone(),
            vec![FileSelection {
                file: row.file.clone(),
                ranges: None,
            }],
            ClassificationTarget::Category(Category::Debug),
            None,
            false,
        );
        let rows = classifications.files_with_mutations(&repository, &previews);
        assert_eq!(
            rows.iter()
                .filter(|row| row.file.path == expected.path)
                .count(),
            1
        );
        assert!(rows.iter().any(|row| row.file.path == expected.path
            && row.category == Some(Category::Debug)
            && row.pending_action.is_none()));
        previews.complete(&[resolved], &Ok(repository.snapshot.clone()));
        assert!(dependency.load(std::sync::atomic::Ordering::Acquire));
        classifications.complete(
            &[next],
            &Err(ClassificationFailure {
                message: "rejected".into(),
                uncertain: false,
            }),
        );
        let rows = classifications.files_with_mutations(&repository, &previews);
        assert!(rows.iter().any(|row| row.file.path == expected.path
            && row.category == Some(Category::Reviewed)
            && !row.file.has_conflict));
        request.result = None;
        let mut deletion = MutationPreviews::default();
        deletion.conflict(&repository, &request);
        assert!(
            ListPreviews::default()
                .files_with_mutations(&repository, &deletion)
                .iter()
                .all(|row| row.file.path != expected.path)
        );
    }

    #[gpui::test(iterations = 3)]
    async fn kk_remote_resolution_can_be_classified_without_capturing_conflict_markers(
        cx: &mut TestAppContext,
        server_cx: &mut TestAppContext,
    ) {
        use crate::kk_panel::remote_tests::{RemoteFixture, run_kk};
        let fixture = RemoteFixture::create(cx, server_cx)
            .await
            .expect("remote fixture");
        let remote = fixture
            .project
            .read_with(cx, |project, cx| project.kk_workbench_client(cx))
            .expect("transport");
        let repository = remote
            .open(&fixture.root, Path::new("kk"), Default::default())
            .await
            .expect("repository");
        let left = repository
            .snapshot(true)
            .await
            .expect("left snapshot")
            .exact_revision("@")
            .await
            .expect("left");
        run_kk(&fixture.root, &["new", "@-"])
            .await
            .expect("other branch");
        std::fs::write(fixture.root.join("file.txt"), "other side\n").expect("other contents");
        run_kk(
            &fixture.root,
            &["rebase", "-r", "@", "-d", &left.full_commit_id],
        )
        .await
        .expect("conflict");
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
        let snapshot = panel.read_with(cx, |panel, _| panel.repositories[0].snapshot.clone());
        let comparison = Comparison::load(
            &snapshot,
            ComparisonIntent::for_target(left.revset(), "left".into(), None),
        )
        .await
        .expect("comparison");
        let conflict = snapshot
            .file_conflict("@", "file.txt")
            .await
            .expect("file conflict");
        assert!(!conflict.binary);
        let (resolved, classified) = panel.update_in(cx, |panel, window, cx| {
            panel.repositories[0].comparison = Some(comparison);
            let resolved = panel.resolve_conflict(
                ConflictRequest {
                    snapshot: snapshot.clone(),
                    conflict,
                    result: Some(b"resolved contents\n".to_vec()),
                    executable: None,
                },
                cx,
            );
            let row = panel
                .list_previews
                .files_with_mutations(&panel.repositories[0], &panel.mutation_previews)
                .into_iter()
                .find(|row| row.file.path == "file.txt")
                .expect("resolved preview");
            assert!(!row.file.has_conflict);
            assert!(row.pending_action.is_some());
            assert!(!row.capture_saved);
            let classified = panel.queue_classification_from(
                row.snapshot,
                row.target.clone(),
                row.target.comparison.clone().expect("intent"),
                vec![FileSelection {
                    file: row.file,
                    ranges: None,
                }],
                ClassificationTarget::Category(Category::Reviewed),
                Vec::new(),
                None,
                row.classification_dependency,
                row.capture_saved,
                cx,
            );
            let row = panel
                .list_previews
                .files_with_mutations(&panel.repositories[0], &panel.mutation_previews)
                .into_iter()
                .find(|row| row.file.path == "file.txt" && row.category == Some(Category::Reviewed))
                .expect("queued category");
            assert!(
                !row.capture_saved,
                "Later classifications must retain the acknowledged source"
            );
            panel.run_file_action(
                FileAction::Classify(ClassificationTarget::Category(Category::Debug)),
                row,
                window,
                cx,
            );
            (resolved, classified)
        });
        resolved.await.expect("resolution");
        classified
            .await
            .map_err(|failure| failure.message)
            .expect("first classification");
        cx.condition(&panel, |panel, _| {
            panel.file_batches.is_empty() && panel.pending_writes == 0 && !panel.loading
        })
        .await;
        panel.read_with(cx, |panel, _| {
            assert!(
                panel.operation_error.is_none(),
                "{:?}",
                panel.operation_error
            )
        });
        let current = snapshot
            .repository()
            .snapshot(false)
            .await
            .expect("current snapshot");
        let working = current.exact_revision("@").await.expect("working revision");
        let categories = current
            .category_commits(&working)
            .await
            .expect("categories");
        let reviewed = categories
            .iter()
            .find(|entry| entry.category == Category::Debug)
            .expect("reviewed resolution");
        assert_eq!(
            current
                .file_content(&reviewed.revision.revset(), "file.txt")
                .await
                .expect("reviewed contents"),
            b"resolved contents\n"
        );
        assert_eq!(
            std::fs::read(fixture.root.join("file.txt")).expect("working contents"),
            b"resolved contents\n"
        );
    }

    #[gpui::test(iterations = 3)]
    async fn kk_remote_move_can_be_classified_before_acknowledgement(
        cx: &mut TestAppContext,
        server_cx: &mut TestAppContext,
    ) {
        run_queued_move_classification(
            cx,
            server_cx,
            false,
            FileAction::Classify(ClassificationTarget::Category(Category::Reviewed)),
        )
        .await;
    }

    #[gpui::test(iterations = 3)]
    async fn kk_remote_failed_move_cancels_dependent_classification(
        cx: &mut TestAppContext,
        server_cx: &mut TestAppContext,
    ) {
        run_queued_move_classification(
            cx,
            server_cx,
            true,
            FileAction::Classify(ClassificationTarget::Category(Category::Reviewed)),
        )
        .await;
    }

    #[gpui::test(iterations = 3)]
    async fn kk_remote_pending_move_can_be_moved_back_to_its_parent(
        cx: &mut TestAppContext,
        server_cx: &mut TestAppContext,
    ) {
        run_queued_move_classification(
            cx,
            server_cx,
            false,
            FileAction::Mutate(FileMutationAction::Move(FileMoveDirection::Parent)),
        )
        .await;
    }

    async fn run_queued_move_classification(
        cx: &mut TestAppContext,
        server_cx: &mut TestAppContext,
        reject_move: bool,
        followup: FileAction,
    ) {
        use crate::kk_panel::remote_tests::{RemoteFixture, run_kk};

        let fixture = RemoteFixture::create(cx, server_cx)
            .await
            .expect("remote fixture");
        std::fs::write(
            fixture.root.join("source-only.txt"),
            "retained range anchor\n",
        )
        .expect("source file");
        run_kk(&fixture.root, &["new", "-m", "work"])
            .await
            .expect("working change");
        std::fs::write(fixture.root.join("other.txt"), "independent\n").expect("independent file");
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
        let snapshot = panel.read_with(cx, |panel, _| panel.repositories[0].snapshot.clone());
        let source = snapshot.exact_revision("@-").await.expect("source");
        let working = snapshot.exact_revision("@").await.expect("working copy");
        let base = snapshot.exact_revision("@--").await.expect("base");
        let comparison = Comparison::load(
            &snapshot,
            ComparisonIntent::for_target(base.revset(), "base".into(), None),
        )
        .await
        .expect("comparison");
        let file = snapshot
            .changed_files(&DiffScope::Change(source.full_commit_id.clone()))
            .await
            .expect("source files")
            .into_iter()
            .find(|file| file.path == "file.txt")
            .expect("file");
        if reject_move {
            std::fs::write(fixture.root.join("file.txt"), "external edit\n")
                .expect("external change");
            snapshot
                .repository()
                .snapshot(true)
                .await
                .expect("record external change");
        }
        let moved = panel.update_in(cx, |panel, window, cx| {
            panel.repositories[0].comparison = Some(comparison.clone());
            let moved = panel.queue_file_mutation_result(
                FileMutationRequest {
                    snapshot: snapshot.clone(),
                    target: super::super::super::kk_diff::DiffTarget::change(&source),
                    file,
                    action: FileMutationAction::Move(FileMoveDirection::Child),
                },
                Vec::new(),
                cx,
            );
            let mut rows = Vec::new();
            panel.mutation_previews.project(
                &panel.repositories[0],
                Some(&super::super::super::kk_diff::DiffTarget::change(&working)),
                &mut rows,
            );
            let row = rows
                .into_iter()
                .find(|row| row.file.path == "file.txt")
                .expect("optimistic destination");
            assert!(row.pending_action.is_some());
            panel.run_file_action(followup, row, window, cx);
            moved
        });
        let outcome = moved.await;
        assert_eq!(outcome.is_err(), reject_move);
        cx.condition(&panel, |panel, _| {
            panel.file_batches.is_empty() && panel.pending_writes == 0 && !panel.loading
        })
        .await;
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.operation_error.is_some(),
                reject_move,
                "{:?}",
                panel.operation_error
            )
        });
        let current = snapshot
            .repository()
            .snapshot(false)
            .await
            .expect("current");
        let categories = Comparison::load(&current, comparison.intent)
            .await
            .expect("current comparison")
            .categories;
        if reject_move {
            assert!(
                categories.is_empty(),
                "A failed move must cancel the queued classification"
            );
            assert_eq!(
                std::fs::read(fixture.root.join("file.txt")).expect("external contents"),
                b"external edit\n"
            );
        } else if matches!(followup, FileAction::Classify(_)) {
            let reviewed = categories
                .iter()
                .find(|entry| entry.category == Category::Reviewed)
                .expect("reviewed destination");
            assert!(reviewed.files.iter().any(|file| file.path == "file.txt"));
            assert_eq!(
                current
                    .file_content(&reviewed.revision.revset(), "file.txt")
                    .await
                    .expect("reviewed contents"),
                b"captured\n"
            );
        } else {
            assert!(categories.is_empty());
            let working = current.exact_revision("@").await.expect("working revision");
            let files = current
                .changed_files(&DiffScope::Change(working.full_commit_id))
                .await
                .expect("working files");
            assert!(files.iter().all(|file| file.path != "file.txt"));
            let source = current
                .exact_revision(&source.revset())
                .await
                .expect("original source");
            assert!(
                current
                    .changed_files(&DiffScope::Change(source.full_commit_id))
                    .await
                    .expect("source files")
                    .iter()
                    .any(|file| file.path == "file.txt")
            );
        }
        assert_eq!(
            std::fs::read(fixture.root.join("other.txt")).expect("independent contents"),
            b"independent\n"
        );
    }

    #[gpui::test]
    async fn optimistic_file_mutations_preserve_independent_intents_and_pending_conflicts(
        cx: &mut TestAppContext,
    ) {
        cx.background_executor.allow_parking();
        let (_directory, mut repository) = cx
            .background_spawn(async { futures_lite::future::block_on(category_repository()) })
            .await
            .expect("repository");
        let comparison = repository.comparison.as_ref().expect("comparison");
        let target = comparison.diff_target();
        let file = comparison
            .files
            .iter()
            .find(|file| file.path == "c.txt")
            .expect("current file")
            .clone();
        let working = repository
            .history
            .revisions
            .iter()
            .find(|revision| revision.is_working_copy)
            .expect("working copy")
            .clone();
        repository.expanded.insert(
            working.full_commit_id.clone(),
            RevisionFiles::Loaded(vec![file.clone()]),
        );
        let mut previews = MutationPreviews::default();
        let revert = previews.file(
            &repository,
            &FileMutationRequest {
                snapshot: repository.snapshot.clone(),
                target,
                file: file.clone(),
                action: FileMutationAction::Revert,
            },
        );
        let mut rows = ListPreviews::default().files(&repository);
        previews.project(&repository, None, &mut rows);
        assert!(
            !rows
                .iter()
                .any(|row| row.file.path == "c.txt" && row.category.is_none())
        );
        assert!(
            rows.iter()
                .any(|row| row.file.path == "a.txt" && !row.pending)
        );

        let reviewed = repository
            .comparison
            .as_ref()
            .expect("comparison")
            .categories
            .iter()
            .find(|category| category.files.iter().any(|file| file.path == "b.txt"))
            .expect("category");
        let moved_file = reviewed.files[0].clone();
        let move_target = super::super::super::kk_diff::DiffTarget::change(&reviewed.revision);
        let destination = repository
            .history
            .revisions
            .iter()
            .find(|revision| {
                reviewed
                    .revision
                    .parent_commit_ids
                    .contains(&revision.full_commit_id)
            })
            .expect("move parent")
            .clone();
        let moved = previews.file(
            &repository,
            &FileMutationRequest {
                snapshot: repository.snapshot.clone(),
                target: move_target,
                file: moved_file.clone(),
                action: FileMutationAction::Move(FileMoveDirection::Parent),
            },
        );
        let destination_target = super::super::super::kk_diff::DiffTarget::change(&destination);
        let mut destination_rows = Vec::new();
        previews.project(
            &repository,
            Some(&destination_target),
            &mut destination_rows,
        );
        assert!(
            destination_rows
                .iter()
                .any(|row| row.file.path == moved_file.path && row.pending)
        );
        previews.complete(
            &[revert],
            &Err(ClassificationFailure {
                message: "rejected".into(),
                uncertain: false,
            }),
        );
        let mut rows = ListPreviews::default().files(&repository);
        previews.project(&repository, None, &mut rows);
        assert!(
            rows.iter()
                .any(|row| row.file.path == "c.txt" && !row.pending)
        );
        destination_rows.clear();
        previews.project(
            &repository,
            Some(&destination_target),
            &mut destination_rows,
        );
        assert!(
            destination_rows
                .iter()
                .any(|row| row.file.path == moved_file.path && row.pending)
        );
        previews.complete(
            &[moved],
            &Err(ClassificationFailure {
                message: "lost reply".into(),
                uncertain: true,
            }),
        );
        previews.settle_uncertain(repository.snapshot.repository().root(), false, false);
        destination_rows.clear();
        previews.project(
            &repository,
            Some(&destination_target),
            &mut destination_rows,
        );
        assert!(destination_rows.is_empty());

        let conflict = kk_workbench::FileConflict {
            repository_root: repository.snapshot.repository().root().to_owned(),
            path: file.path,
            change_id: working.full_change_id,
            commit_id: working.full_commit_id,
            immutable: false,
            inputs: Vec::new(),
            materialized: Vec::new(),
            marker_length: 7,
            binary: false,
        };
        let resolved = previews.conflict(
            &repository,
            &ConflictRequest {
                snapshot: repository.snapshot.clone(),
                conflict,
                result: Some(b"resolved\n".to_vec()),
                executable: None,
            },
        );
        let mut rows = ListPreviews::default().files(&repository);
        previews.project(&repository, None, &mut rows);
        assert!(
            rows.iter()
                .any(|row| row.file.path == "c.txt" && row.pending && !row.file.has_conflict)
        );
        previews.complete(&[resolved], &Ok(repository.snapshot.clone()));
        let confirmed = previews.confirmed();
        previews.refreshed(
            &confirmed,
            std::slice::from_ref(&repository),
            &HashSet::new(),
        );
        let mut rows = ListPreviews::default().files(&repository);
        previews.project(&repository, None, &mut rows);
        assert!(
            rows.iter()
                .any(|row| row.file.path == "c.txt" && row.pending),
            "failed comparison refresh must retain projection"
        );
        let loaded = HashSet::from([repository.snapshot.repository().root().to_owned()]);
        previews.refreshed(&confirmed, std::slice::from_ref(&repository), &loaded);
        let mut rows = ListPreviews::default().files(&repository);
        previews.project(&repository, None, &mut rows);
        assert!(rows.iter().all(|row| !row.pending));
    }

    #[gpui::test]
    async fn optimistic_lists_reconcile_only_matching_intents(cx: &mut TestAppContext) {
        cx.background_executor.allow_parking();
        let (_directory, repository) = cx
            .background_spawn(async { futures_lite::future::block_on(category_repository()) })
            .await
            .expect("category repository");
        let comparison = repository.comparison.as_ref().expect("comparison");
        let mut previews = ListPreviews::default();
        let current = comparison
            .files
            .iter()
            .find(|file| file.path == "c.txt")
            .expect("current file")
            .clone();
        let reviewed = comparison
            .categories
            .iter()
            .find(|entry| entry.files.iter().any(|file| file.path == "a.txt"))
            .expect("reviewed source");
        let source = super::super::super::kk_diff::DiffTarget::change(&reviewed.revision);
        let first = previews.push(
            repository.snapshot.clone(),
            &comparison.intent,
            comparison.diff_target(),
            vec![FileSelection {
                file: current,
                ranges: None,
            }],
            ClassificationTarget::Category(Category::Debug),
        );
        let second = previews.push(
            repository.snapshot.clone(),
            &comparison.intent,
            source,
            vec![FileSelection {
                file: reviewed.files[0].clone(),
                ranges: None,
            }],
            ClassificationTarget::Comparison,
        );
        let projected = previews.files(&repository);
        let base = repository
            .history
            .revisions
            .iter()
            .find(|revision| revision.revset() == comparison.intent.target_revset)
            .expect("comparison base");
        assert!(
            !previews.hides(&repository, base, &comparison.files[0]),
            "comparison classification must not hide the excluded base's files"
        );
        assert!(projected.iter().any(|row| row.file.path == "c.txt"
            && row.category == Some(Category::Debug)
            && row.pending));
        assert!(
            !projected
                .iter()
                .any(|row| row.file.path == "c.txt" && row.category.is_none())
        );
        assert!(
            projected
                .iter()
                .any(|row| row.file.path == "a.txt" && row.category.is_none() && row.pending)
        );
        assert!(
            projected
                .iter()
                .any(|row| row.file.path == "b.txt" && !row.pending)
        );
        let rejected_source = projected
            .iter()
            .find(|row| row.file.path == "a.txt" && row.category.is_none())
            .expect("pending source")
            .clone();

        // A refresh that began before acknowledgement must not retire the later result.
        let earlier_refresh = previews.confirmed();
        previews.complete(&[first], &Ok(repository.snapshot.clone()));
        previews.refreshed(
            &earlier_refresh,
            std::slice::from_ref(&repository),
            &HashSet::from([repository.snapshot.repository().root().to_owned()]),
        );
        assert!(
            previews
                .files(&repository)
                .iter()
                .any(|row| row.file.path == "c.txt" && row.pending)
        );
        previews.complete(
            &[second],
            &Err(ClassificationFailure {
                message: "rejected".into(),
                uncertain: false,
            }),
        );
        let (completion, _completed) = futures::channel::oneshot::channel();
        let mut stale_action = ClassificationJob::new(
            repository.snapshot.clone(),
            rejected_source.target,
            comparison.intent.clone(),
            vec![FileSelection {
                file: rejected_source.file,
                ranges: None,
            }],
            ClassificationTarget::Category(Category::Debug),
            completion,
            Vec::new(),
        )
        .expect("stale action");
        stale_action.source_category = Some(ClassificationTarget::Comparison);
        stale_action.source_dependency = rejected_source.classification_dependency;
        let mut queue = RepositoryQueue::default();
        queue.enqueue(stale_action);
        let (queued, current) = queue.next().expect("queued stale action");
        let failure = queued.check_dependencies().expect_err(
            "a stale row must retain its failed prerequisite after it leaves the queue",
        );
        let super::super::super::kk_classification::RepositoryJob::Classification(job) = queued.job
        else {
            panic!("classification job");
        };
        assert!(
            cx.background_spawn(ClassificationJob::finish(current, job.saved, Err(failure)))
                .await
                .is_err()
        );
        let projected = previews.files(&repository);
        assert!(projected.iter().any(|row| row.file.path == "a.txt"
            && row.category == Some(Category::Reviewed)
            && !row.pending));
        assert!(projected.iter().any(|row| row.file.path == "c.txt"
            && row.category == Some(Category::Debug)
            && row.pending));

        let refreshed = previews.confirmed();
        previews.refreshed(
            &refreshed,
            std::slice::from_ref(&repository),
            &HashSet::new(),
        );
        assert_eq!(
            previews.confirmed(),
            refreshed,
            "a failed comparison read cannot confirm the list"
        );
        let mut other_lifetime = repository.clone();
        other_lifetime.snapshot = other_lifetime
            .snapshot
            .with_lifetime(Some(kk_workbench::RepositoryLifetime::default()));
        previews.refreshed(
            &refreshed,
            &[other_lifetime],
            &HashSet::from([repository.snapshot.repository().root().to_owned()]),
        );
        assert!(!previews.confirmed().is_empty());

        previews.complete(
            &[first],
            &Err(ClassificationFailure {
                message: "connection lost".into(),
                uncertain: true,
            }),
        );
        assert!(
            previews
                .files(&repository)
                .iter()
                .any(|row| row.file.path == "c.txt" && row.pending)
        );
        previews.settle_uncertain(repository.snapshot.repository().root(), false, true);
        assert!(previews.files(&repository).iter().all(|row| !row.pending));

        let first = previews.push(
            repository.snapshot.clone(),
            &comparison.intent,
            comparison.diff_target(),
            vec![FileSelection {
                file: comparison.files[0].clone(),
                ranges: None,
            }],
            ClassificationTarget::Category(Category::Rejected),
        );
        previews.push(
            repository.snapshot.clone(),
            &comparison.intent,
            super::super::super::kk_diff::DiffTarget::change(&reviewed.revision),
            vec![FileSelection {
                file: reviewed.files[0].clone(),
                ranges: Some(vec![kk_workbench::LineRange {
                    side: kk_workbench::SelectionSide::Modified,
                    begin: 1,
                    end: 1,
                }]),
            }],
            ClassificationTarget::Category(Category::Debug),
        );
        previews.complete(
            &[first],
            &Err(ClassificationFailure {
                message: "earlier write rejected".into(),
                uncertain: false,
            }),
        );
        let projected = previews.files(&repository);
        assert!(
            projected
                .iter()
                .any(|row| row.file.path == "c.txt" && row.category.is_none() && !row.pending)
        );
        assert!(projected.iter().any(|row| row.file.path == "a.txt"
            && row.category == Some(Category::Debug)
            && row.pending));
        assert!(
            projected.iter().any(|row| row.file.path == "a.txt"
                && row.category == Some(Category::Reviewed)
                && row.pending),
            "a partial transfer must preserve the source file"
        );

        let file = projected
            .iter()
            .find(|row| row.category == Some(Category::Debug))
            .expect("pending destination")
            .clone();
        let row = TreeRow {
            id: TreeRowId::File(file.id.clone()),
            parent: None,
            fallback: None,
            depth: 0,
            expanded: None,
            kind: TreeRowKind::File(file.clone()),
            render_index: 0,
        };
        let mut selection = TreeSelectionState::default();
        selection.reconcile(vec![row.clone()]);
        selection.select(&row.id, false, false);
        let mut confirmed = file;
        confirmed.id.source = "assigned-category-change".into();
        confirmed.pending = false;
        let confirmed_id = TreeRowId::File(confirmed.id.clone());
        selection.reconcile(vec![TreeRow {
            id: confirmed_id.clone(),
            kind: TreeRowKind::File(confirmed),
            ..row
        }]);
        assert_eq!(selection.selected, HashSet::from([confirmed_id.clone()]));
        assert_eq!(selection.cursor, Some(confirmed_id.clone()));
        assert_eq!(selection.anchor, Some(confirmed_id));

        previews.forget_root(repository.snapshot.repository().root());
        for (source, destination) in [
            (None, ClassificationTarget::Category(Category::Reviewed)),
            (
                Some(ClassificationTarget::Category(Category::Reviewed)),
                ClassificationTarget::Category(Category::Rejected),
            ),
            (
                Some(ClassificationTarget::Category(Category::Rejected)),
                ClassificationTarget::Comparison,
            ),
        ] {
            previews.push_from(
                repository.snapshot.clone(),
                &comparison.intent,
                comparison.diff_target(),
                vec![FileSelection {
                    file: comparison.files[0].clone(),
                    ranges: None,
                }],
                destination,
                source,
                true,
            );
            let projected = previews.files(&repository);
            let file = projected
                .iter()
                .find(|file| file.file.path == "c.txt")
                .expect("latest classification");
            assert!(file.pending);
            assert_eq!(
                file.category,
                match destination {
                    ClassificationTarget::Category(category) => Some(category),
                    ClassificationTarget::Comparison => None,
                }
            );
            assert_eq!(
                projected
                    .iter()
                    .filter(|file| file.file.path == "c.txt")
                    .count(),
                1
            );
        }
    }

    async fn click_file_row(
        panel: &Entity<KkPanel>,
        path: &str,
        click_count: usize,
        cx: &mut VisualTestContext,
    ) {
        cx.update(|window, cx| {
            window.refresh();
            window.draw(cx).clear(cx);
        });
        let position = panel.read_with(cx, |panel, _| {
            let row = panel.tree_selection.rows.iter().find(|row| {
                matches!(&row.kind, TreeRowKind::File(file) if file.file.path == path && file.id.section.starts_with("comparison:"))
            }).expect("visible comparison file");
            let scroll = panel.scroll_handle.0.borrow();
            let bounds = scroll.base_handle.bounds();
            let height = scroll.last_item_size.expect("rendered rows").contents.height
                / panel.tree_selection.rendered.len() as f32;
            bounds.origin + scroll.base_handle.offset()
                + gpui::point(bounds.size.width / 2., height * (row.render_index as f32 + 0.5))
        });
        cx.simulate_event(gpui::MouseDownEvent {
            position,
            modifiers: Modifiers::none(),
            button: gpui::MouseButton::Left,
            click_count,
            first_mouse: false,
        });
        cx.simulate_event(gpui::MouseUpEvent {
            position,
            modifiers: Modifiers::none(),
            button: gpui::MouseButton::Left,
            click_count,
        });
        panel
            .update(cx, |panel, _| panel.load_tasks.remove("open-diff"))
            .expect("file click starts opening")
            .await;
        cx.run_until_parked();
    }

    fn click_disclosure(panel: &Entity<KkPanel>, id: &TreeRowId, cx: &mut VisualTestContext) {
        cx.update(|window, cx| {
            window.refresh();
            window.draw(cx).clear(cx);
        });
        let graph_width = cx
            .debug_bounds("kk-history-graph")
            .map_or(px(0.), |bounds| bounds.size.width);
        let position = panel.read_with(cx, |panel, _| {
            let row = panel
                .tree_selection
                .rows
                .iter()
                .find(|row| &row.id == id)
                .expect("expandable row");
            let scroll = panel.scroll_handle.0.borrow();
            let bounds = scroll.base_handle.bounds();
            let height = scroll
                .last_item_size
                .expect("rendered rows")
                .contents
                .height
                / panel.tree_selection.rendered.len() as f32;
            bounds.origin
                + scroll.base_handle.offset()
                + gpui::point(
                    px((row.depth + 1) as f32 * 12. - 6.)
                        + if panel.mode == HistoryMode::Log
                            && matches!(row.kind, TreeRowKind::Revision(_, _))
                        {
                            graph_width
                        } else {
                            px(0.)
                        },
                    height * (row.render_index as f32 + 0.5),
                )
        });
        cx.simulate_mouse_move(position, None, Modifiers::none());
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.simulate_click(position, Modifiers::none());
    }

    fn hover_tree_row(
        panel: &Entity<KkPanel>,
        id: &TreeRowId,
        cx: &mut VisualTestContext,
    ) -> gpui::Point<gpui::Pixels> {
        panel.update(cx, |panel, cx| {
            panel.update_tree(cx);
            let row = panel
                .tree_selection
                .rows
                .iter()
                .find(|row| &row.id == id)
                .expect("visible row");
            panel
                .scroll_handle
                .scroll_to_item(row.render_index, gpui::ScrollStrategy::Nearest);
            cx.notify();
        });
        cx.update(|window, cx| {
            window.refresh();
            window.draw(cx).clear(cx);
        });
        let position = panel.read_with(cx, |panel, _| {
            let row = panel
                .tree_selection
                .rows
                .iter()
                .find(|row| &row.id == id)
                .expect("rendered row");
            let scroll = panel.scroll_handle.0.borrow();
            let bounds = scroll.base_handle.bounds();
            let height = scroll
                .last_item_size
                .expect("rendered rows")
                .contents
                .height
                / panel.tree_selection.rendered.len() as f32;
            bounds.origin
                + scroll.base_handle.offset()
                + gpui::point(
                    bounds.size.width / 2.,
                    height * (row.render_index as f32 + 0.5),
                )
        });
        cx.simulate_mouse_move(position, None, Modifiers::none());
        cx.update(|window, cx| window.draw(cx).clear(cx));
        position
    }

    fn click_row_icon(
        panel: &Entity<KkPanel>,
        id: &TreeRowId,
        selector: &'static str,
        cx: &mut VisualTestContext,
    ) {
        let position = hover_tree_row(panel, id, cx);
        let icon = cx.debug_bounds(selector).expect(selector);
        cx.simulate_click(gpui::point(icon.center().x, position.y), Modifiers::none());
    }

    fn click_icon(selector: &'static str, cx: &mut VisualTestContext) {
        cx.update(|window, cx| {
            window.refresh();
            window.draw(cx).clear(cx);
        });
        let bounds = cx.debug_bounds(selector).expect("toolbar icon");
        cx.simulate_mouse_move(bounds.center(), None, Modifiers::none());
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.simulate_click(bounds.center(), Modifiers::none());
        for _ in 0..3 {
            cx.update(|window, cx| {
                window.simulate_next_frame(cx);
                window.draw(cx).clear(cx);
            });
        }
    }

    fn right_click_revision(
        panel: &Entity<KkPanel>,
        root: &Path,
        revision: &Revision,
        cx: &mut VisualTestContext,
    ) {
        let id = TreeRowId::Revision(root.to_owned(), revision.full_change_id.clone());
        panel.update(cx, |panel, cx| {
            panel.update_tree(cx);
            let row = panel
                .tree_selection
                .rows
                .iter()
                .find(|row| row.id == id)
                .expect("visible revision");
            panel
                .scroll_handle
                .scroll_to_item(row.render_index, gpui::ScrollStrategy::Nearest);
            cx.notify();
        });
        cx.update(|window, cx| {
            window.refresh();
            window.draw(cx).clear(cx);
        });
        let position = panel.read_with(cx, |panel, _| {
            let row = panel
                .tree_selection
                .rows
                .iter()
                .find(|row| row.id == id)
                .expect("revision row");
            let scroll = panel.scroll_handle.0.borrow();
            let height = scroll
                .last_item_size
                .expect("rendered rows")
                .contents
                .height
                / panel.tree_selection.rendered.len() as f32;
            let bounds = scroll.base_handle.bounds();
            bounds.origin
                + scroll.base_handle.offset()
                + gpui::point(
                    bounds.size.width / 2.,
                    height * (row.render_index as f32 + 0.5),
                )
        });
        cx.simulate_mouse_down(position, gpui::MouseButton::Right, Modifiers::none());
        cx.simulate_mouse_up(position, gpui::MouseButton::Right, Modifiers::none());
        assert!(panel.read_with(cx, |panel, _| panel.context_menu.is_some()));
        cx.update(|window, cx| window.draw(cx).clear(cx));
    }

    #[gpui::test(iterations = 3)]
    async fn inline_file_actions_and_toolbar_use_the_clicked_entry(cx: &mut TestAppContext) {
        cx.background_executor.allow_parking();
        let (directory, mut repository) = cx
            .background_spawn(async { futures_lite::future::block_on(category_repository()) })
            .await
            .expect("repository");
        let root = repository.snapshot.repository().root().to_owned();
        let working_copy = repository
            .history
            .revisions
            .iter()
            .find(|revision| revision.is_working_copy)
            .expect("working copy")
            .clone();
        let files = cx
            .background_spawn({
                let snapshot = repository.snapshot.clone();
                let scope = DiffScope::Change(working_copy.full_commit_id.clone());
                async move { snapshot.changed_files(&scope).await }
            })
            .await
            .expect("working-copy files");
        repository.expanded.insert(
            working_copy.full_commit_id.clone(),
            RevisionFiles::Loaded(files),
        );
        cx.update(|cx| {
            let mut settings = SettingsStore::test(cx);
            settings
                .set_user_settings(r#"{"kk_workbench":{"auto_refresh":false}}"#, cx)
                .expect("settings");
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            cx.bind_keys([gpui::KeyBinding::new("escape", menu::Cancel, Some("menu"))]);
            init(cx);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(&root, serde_json::json!({"a.txt":"first category\n", "b.txt":"second category\n", "c.txt":"current work\n"})).await;
        let project = Project::test(fs, [root.as_path()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = multi_workspace.read_with(cx, |view, _| view.workspace().clone());
        let panel = cx
            .update(|window, cx| KkPanel::load(workspace.downgrade(), window.to_async(cx)))
            .await
            .expect("panel");
        let (file_id, selection) = panel.update(cx, |panel, cx| {
            panel.repositories = vec![repository];
            panel
                .tree_selection
                .expanded
                .insert(TreeRowId::Category(root.clone(), "Reviewed"), true);
            panel.update_tree(cx);
            let file = panel
                .visible_files
                .iter()
                .find(|file| file.file.path == "c.txt" && file.id.section.starts_with("commit:"))
                .expect("commit file");
            let file_id = TreeRowId::File(file.id.clone());
            let other = panel
                .visible_files
                .iter()
                .find(|file| file.file.path == "a.txt")
                .expect("other selected file");
            let other_id = TreeRowId::File(other.id.clone());
            panel.select_tree_row(&other_id, false, false, cx);
            panel.select_tree_row(&file_id, true, false, cx);
            (file_id, panel.tree_selection.selected.clone())
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_panel(panel.clone(), window, cx);
            workspace.focus_panel::<KkPanel>(window, cx);
        });
        click_row_icon(&panel, &file_id, "ICON-File", cx);
        panel.read_with(cx, |panel, _| {
            let (_, label, rows) = panel.opening_feedback.as_ref().expect("opening feedback");
            assert!(label.contains("c.txt"));
            assert_eq!(rows, &HashSet::from([file_id.clone()]));
        });
        cx.run_until_parked();
        cx.condition(&panel, |panel, _| panel.file_batches.is_empty())
            .await;
        panel.read_with(cx, |panel, _| {
            assert!(panel.error.is_none(), "opening error: {:?}", panel.error)
        });
        workspace.read_with(cx, |workspace, cx| {
            let pane = workspace.active_pane().read(cx);
            assert_eq!(
                pane.items_len(),
                1,
                "inline Open must ignore the other selected file"
            );
            let editor = pane
                .active_item()
                .and_then(|item| item.downcast::<editor::Editor>())
                .expect("plain file editor");
            assert_eq!(editor.read(cx).buffer().read(cx).title(cx), "c.txt");
        });
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.tree_selection.selected, selection);
            assert!(
                !panel.load_tasks.contains_key("open-diff"),
                "inline Open must not also preview the row"
            );
        });
        click_row_icon(&panel, &file_id, "ICON-Undo", cx);
        assert!(cx.has_pending_prompt());
        assert!(
            cx.pending_prompt()
                .expect("revert confirmation")
                .0
                .contains("1 selected file(s)")
        );
        cx.simulate_prompt_answer("Cancel");
        cx.condition(&panel, |panel, _| panel.file_batches.is_empty())
            .await;
        click_row_icon(&panel, &file_id, "ICON-ArrowDown", cx);
        cx.condition(&panel, |panel, _| {
            panel.file_batches.is_empty() && !panel.loading && panel.pending_writes == 0
        })
        .await;
        let snapshot = panel.read_with(cx, |panel, _| panel.repositories[0].snapshot.clone());
        let current_files = cx
            .background_spawn(async move {
                let revision = snapshot.exact_revision("@").await?;
                snapshot
                    .changed_files(&DiffScope::Change(revision.full_commit_id))
                    .await
            })
            .await
            .expect("updated working copy");
        assert!(
            current_files.is_empty(),
            "Move to Parent must move the clicked working-copy file"
        );
        assert!(panel.read_with(cx, |panel, _| panel.operation_error.is_none()));

        let parent = panel.read_with(cx, |panel, _| {
            let repository = &panel.repositories[0];
            let current = repository
                .history
                .revisions
                .iter()
                .find(|revision| revision.is_working_copy)
                .expect("working copy");
            repository
                .history
                .revisions
                .iter()
                .find(|revision| current.parent_commit_ids.contains(&revision.full_commit_id))
                .expect("parent revision")
                .clone()
        });
        click_disclosure(
            &panel,
            &TreeRowId::Revision(root.clone(), parent.full_change_id.clone()),
            cx,
        );
        cx.condition(&panel, |panel, _| {
            matches!(
                panel.repositories[0].expanded.get(&parent.full_commit_id),
                Some(RevisionFiles::Loaded(_))
            )
        })
        .await;
        let child_file = panel.update(cx, |panel, cx| {
            panel.update_tree(cx);
            let file = panel
                .visible_files
                .iter()
                .find(|file| file.file.path == "c.txt" && file.id.section.starts_with("commit:") && matches!(&file.target.scope, DiffScope::Change(commit) if *commit == parent.full_commit_id))
                .expect("moved file in parent revision");
            TreeRowId::File(file.id.clone())
        });
        click_row_icon(&panel, &child_file, "ICON-ArrowUp", cx);
        cx.condition(&panel, |panel, _| {
            panel.file_batches.is_empty() && !panel.loading && panel.pending_writes == 0
        })
        .await;
        let snapshot = panel.read_with(cx, |panel, _| panel.repositories[0].snapshot.clone());
        let current_files = cx
            .background_spawn(async move {
                let revision = snapshot.exact_revision("@").await?;
                snapshot
                    .changed_files(&DiffScope::Change(revision.full_commit_id))
                    .await
            })
            .await
            .expect("file moved to child");
        assert_eq!(
            current_files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            ["c.txt"]
        );
        assert!(panel.read_with(cx, |panel, _| panel.operation_error.is_none()));

        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
        pane.update_in(cx, |pane, window, cx| {
            pane.close_active_item(&Default::default(), window, cx)
        })
        .await
        .expect("close the verified clean editor before toolbar checks");
        click_row_icon(
            &panel,
            &TreeRowId::Comparison(root.clone()),
            "ICON-Close",
            cx,
        );
        assert!(panel.read_with(cx, |panel, _| panel.repositories[0].comparison.is_none()));
        click_icon("ICON-ListCollapse", cx);

        cx.update(|window, cx| window.draw(cx).clear(cx));
        let (bounds, offset, selection) = panel.read_with(cx, |panel, _| {
            let handle = panel.scroll_handle.0.borrow();
            (
                handle.base_handle.bounds(),
                handle.base_handle.offset(),
                panel.tree_selection.selected.clone(),
            )
        });
        panel.update(cx, |panel, cx| {
            panel.loading = true;
            cx.notify();
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        panel.read_with(cx, |panel, _| {
            let handle = panel.scroll_handle.0.borrow();
            assert_eq!(
                handle.base_handle.bounds(),
                bounds,
                "background loading must not move or resize the list"
            );
            assert_eq!(handle.base_handle.offset(), offset);
            assert_eq!(panel.tree_selection.selected, selection);
        });
        panel.update(cx, |panel, cx| {
            panel.loading = false;
            cx.notify();
        });

        panel.update_in(cx, |panel, window, cx| window.focus(&panel.focus, cx));
        click_icon("ICON-Ellipsis", cx);
        assert!(
            cx.debug_bounds("MENU_ITEM-Sort by File Name").is_some()
                || cx.debug_bounds("MENU_ITEM-Use Repository Order").is_some(),
            "More Actions must open before Escape"
        );
        cx.simulate_keystrokes("escape");
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(
            cx.update(|window, cx| panel.read(cx).focus.is_focused(window)),
            "cancelling More Actions must return keyboard focus to the list"
        );

        panel.update(cx, |panel, cx| {
            panel
                .file_order
                .set(FileOrder::Repository, cx, KkPanel::show_error)
        });
        click_icon("ICON-Ellipsis", cx);
        let sort = cx
            .debug_bounds("MENU_ITEM-Sort by File Name")
            .expect("sorting in More Actions");
        assert!(cx.debug_bounds("MENU_ITEM-Use Repository Order").is_none());
        cx.simulate_click(sort.center(), Modifiers::none());
        assert!(panel.read_with(cx, |panel, _| panel.file_order.order()) == FileOrder::FileName);
        click_icon("ICON-Ellipsis", cx);
        let sort = cx
            .debug_bounds("MENU_ITEM-Use Repository Order")
            .expect("opposite sorting action");
        cx.simulate_click(sort.center(), Modifiers::none());
        assert!(panel.read_with(cx, |panel, _| panel.file_order.order()) == FileOrder::Repository);
        panel.update(cx, |panel, cx| {
            panel.select_tree_row(&TreeRowId::History(root.clone()), false, false, cx);
        });
        click_icon("ICON-MagnifyingGlass", cx);
        assert!(
            workspace.read_with(cx, |workspace, cx| workspace
                .active_modal::<picker::Picker<kk_revision_picker::RevisionDelegate>>(cx)
                .is_some()),
            "single-root Find Change is available in the toolbar"
        );
        cx.dispatch_action(menu::Cancel);
        panel.update_in(cx, |panel, window, cx| window.focus(&panel.focus, cx));
        let mode_position = cx
            .debug_bounds("ICON-Crosshair")
            .expect("history mode control")
            .center();
        click_icon("ICON-Crosshair", cx);
        cx.condition(&panel, |panel, _| !panel.loading).await;
        assert_eq!(panel.read_with(cx, |panel, _| panel.mode), HistoryMode::Log);
        cx.simulate_click(mode_position, Modifiers::none());
        cx.condition(&panel, |panel, _| !panel.loading).await;
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.mode),
            HistoryMode::Lineage
        );
        click_icon("ICON-RotateCw", cx);
        cx.condition(&panel, |panel, _| !panel.loading).await;
        click_icon("ICON-ListCollapse", cx);
        panel.read_with(cx, |panel, _| {
            assert!(
                panel
                    .tree_selection
                    .expanded
                    .values()
                    .all(|expanded| !expanded)
            )
        });
        let revision = panel.update(cx, |panel, cx| {
            panel
                .tree_selection
                .expanded
                .insert(TreeRowId::History(root.clone()), true);
            let revision = panel.repositories[0]
                .history
                .revisions
                .iter()
                .find(|revision| revision.is_working_copy)
                .expect("current revision")
                .clone();
            panel.repositories[0].expanded.insert(
                revision.full_commit_id.clone(),
                RevisionFiles::Failed("test loading failure".into()),
            );
            panel.update_tree(cx);
            revision
        });
        let revision_id = TreeRowId::Revision(root.clone(), revision.full_change_id.clone());
        let position = hover_tree_row(&panel, &revision_id, cx);
        let height = panel.read_with(cx, |panel, _| {
            panel
                .scroll_handle
                .0
                .borrow()
                .last_item_size
                .expect("rows")
                .contents
                .height
                / panel.tree_selection.rendered.len() as f32
        });
        let error_position = gpui::point(position.x, position.y + height);
        cx.simulate_mouse_move(error_position, None, Modifiers::none());
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let output = cx
            .debug_bounds("ICON-Terminal")
            .expect("Show Output on load error");
        cx.simulate_click(output.center(), Modifiers::none());
        workspace.read_with(cx, |workspace, cx| {
            assert!(
                workspace
                    .dock_at_position(DockPosition::Bottom)
                    .read(cx)
                    .is_open()
            )
        });
        let position = hover_tree_row(&panel, &revision_id, cx);
        let error_position = gpui::point(position.x, position.y + height);
        cx.simulate_mouse_down(error_position, gpui::MouseButton::Right, Modifiers::none());
        cx.simulate_mouse_up(error_position, gpui::MouseButton::Right, Modifiers::none());
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.simulate_keystrokes("escape");
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(panel.read_with(cx, |panel, _| panel.context_menu.is_none()));
        assert!(
            cx.update(|window, cx| panel.read(cx).focus.is_focused(window)),
            "cancelling the load-error menu must restore list focus"
        );
        cx.simulate_mouse_down(error_position, gpui::MouseButton::Right, Modifiers::none());
        cx.simulate_mouse_up(error_position, gpui::MouseButton::Right, Modifiers::none());
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let retry = cx
            .debug_bounds("MENU_ITEM-Retry Loading Commit")
            .expect("error row retains Retry in its context menu");
        cx.simulate_click(retry.center(), Modifiers::none());
        cx.condition(&panel, |panel, _| {
            matches!(
                panel.repositories[0].expanded.get(&revision.full_commit_id),
                Some(RevisionFiles::Loaded(_))
            )
        })
        .await;
        drop(directory);
    }

    #[gpui::test(iterations = 3)]
    async fn file_clicks_preview_without_stealing_focus_and_keep_confirmed_or_edited_tabs(
        cx: &mut TestAppContext,
    ) {
        use crate::kk_panel::remote_tests::run_kk;
        use workspace::Item;

        cx.background_executor.allow_parking();
        let (directory, repository) = cx
            .background_spawn(async {
                futures_lite::future::block_on(async {
                    let directory = tempfile::tempdir()?;
                    run_kk(directory.path(), &["init", "."]).await?;
                    for path in ["a.txt", "b.txt", "c.txt"] {
                        std::fs::write(directory.path().join(path), "base\n")?;
                    }

                    run_kk(directory.path(), &["new", "-m", "work"]).await?;
                    for (path, content) in [
                        ("a.txt", "first category\n"),
                        ("b.txt", "second category\n"),
                        ("c.txt", "current work\n"),
                    ] {
                        std::fs::write(directory.path().join(path), content)?;
                    }

                    let repository =
                        kk_workbench::Repository::open(directory.path(), Path::new("kk")).await?;
                    let snapshot = repository.snapshot(true).await?;
                    let base = snapshot.exact_revision("@-").await?;
                    let history = snapshot.history(HistoryMode::Lineage, 50).await?;
                    let comparison = Comparison::load(
                        &snapshot,
                        ComparisonIntent::for_target(base.revset(), "base".into(), None),
                    )
                    .await?;
                    anyhow::Ok((
                        directory,
                        RepositoryView {
                            stale: false,
                            snapshot,
                            history,
                            expanded: HashMap::new(),
                            comparison: Some(comparison),
                            located: None,
                        },
                    ))
                })
            })
            .await
            .expect("file history");
        let root = repository.snapshot.repository().root().to_owned();
        cx.update(|cx| {
            let mut settings = SettingsStore::test(cx);
            settings
                .set_user_settings(r#"{"kk_workbench":{"auto_refresh":false}}"#, cx)
                .expect("settings");
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            init(cx);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            &root,
            serde_json::json!({
                "a.txt": "first category\n", "b.txt": "second category\n", "c.txt": "current work\n"
            }),
        )
        .await;
        let project = Project::test(fs, [root.as_path()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |view, _| view.workspace().clone());
        let panel = cx
            .update(|window, cx| KkPanel::load(workspace.downgrade(), window.to_async(cx)))
            .await
            .expect("panel");
        panel.update(cx, |panel, cx| {
            panel.repositories = vec![repository];
            panel.update_tree(cx);
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_panel(panel.clone(), window, cx);
            workspace.focus_panel::<KkPanel>(window, cx);
        });
        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
        let focus = panel.read_with(cx, |panel, _| panel.focus.clone());

        let failed = panel.update_in(cx, |panel, window, cx| {
            let file = panel
                .visible_files
                .iter()
                .find(|file| file.file.path == "c.txt")
                .expect("file")
                .clone();
            let mut target = file.target;
            target.scope = DiffScope::Change("missing-revision".into());
            panel.open_file(
                0,
                target,
                file.file,
                true,
                DiffOpenMode::Preview,
                window,
                cx,
            );
            panel
                .load_tasks
                .remove("open-diff")
                .expect("failed open task")
        });
        failed.await;
        assert!(panel.read_with(cx, |panel, _| panel.error.is_some()));

        click_file_row(&panel, "c.txt", 1, cx).await;
        let first = pane.read_with(cx, |pane, _| {
            assert_eq!(pane.items_len(), 1);
            pane.preview_item_id().expect("single-click preview")
        });
        assert!(cx.update(|window, _| focus.is_focused(window)));
        assert!(panel.read_with(cx, |panel, _| panel.error.is_none()));

        click_file_row(&panel, "a.txt", 1, cx).await;
        let second = pane.read_with(cx, |pane, _| {
            assert_eq!(pane.items_len(), 1);
            pane.preview_item_id().expect("replacement preview")
        });
        assert_ne!(first, second);
        assert!(cx.update(|window, _| focus.is_focused(window)));
        click_file_row(&panel, "a.txt", 2, cx).await;
        pane.read_with(cx, |pane, _| {
            assert_eq!(pane.items_len(), 1);
            assert!(pane.preview_item_id().is_none());
        });
        assert!(!cx.update(|window, _| focus.is_focused(window)));

        click_file_row(&panel, "c.txt", 1, cx).await;
        let buffer = project
            .update(cx, |project, cx| {
                project.open_local_buffer(root.join("c.txt"), cx)
            })
            .await
            .expect("shared live buffer");
        buffer.update(cx, |buffer, cx| {
            buffer.edit([(0..0, "draft\n")], None, cx);
        });
        cx.run_until_parked();
        assert!(pane.read_with(cx, |pane, _| pane.preview_item_id().is_none()));
        click_file_row(&panel, "b.txt", 1, cx).await;
        pane.read_with(cx, |pane, _| assert_eq!(pane.items_len(), 3));
        assert_eq!(
            buffer.read_with(cx, |buffer, _| buffer.text()),
            "draft\ncurrent work\n"
        );
        assert!(buffer.read_with(cx, |buffer, _| buffer.is_dirty()));

        cx.simulate_keystrokes("enter");
        panel
            .update(cx, |panel, _| panel.load_tasks.remove("open-diff"))
            .expect("confirmed open")
            .await;
        assert!(pane.read_with(cx, |pane, _| pane.preview_item_id().is_none()));
        assert!(!cx.update(|window, _| focus.is_focused(window)));
        assert!(workspace.read_with(cx, |workspace, cx| {
            workspace
                .items_of_type::<KkDiff>(cx)
                .all(|view| view.read(cx).can_save(cx))
        }));
        click_file_row(&panel, "a.txt", 2, cx).await;
        let (history, history_label) = panel.update_in(cx, |panel, window, cx| {
            let revision = panel.repositories[0]
                .history
                .revisions
                .iter()
                .find(|revision| revision.is_working_copy)
                .expect("working copy")
                .clone();
            let file = panel
                .visible_files
                .iter()
                .find(|file| file.file.path == "a.txt")
                .expect("permanent file")
                .file
                .clone();
            panel.open_file(
                0,
                super::super::super::kk_diff::DiffTarget::change(&revision),
                file,
                true,
                DiffOpenMode::Preview,
                window,
                cx,
            );
            let task = panel
                .load_tasks
                .remove("open-diff")
                .expect("same-file history preview");
            (task, format!("a.txt ({})", revision.change_id))
        });
        history.await;
        pane.read_with(cx, |pane, _| {
            assert_eq!(pane.items_len(), 3);
            assert!(
                pane.preview_item_id().is_none(),
                "changing the diff scope must keep a permanent file permanent"
            );
        });
        workspace.read_with(cx, |workspace, cx| {
            let active = workspace
                .active_item_as::<KkDiff>(cx)
                .expect("requested diff is displayed");
            assert_eq!(
                active.read(cx).tab_content_text(0, cx).as_ref(),
                history_label
            );
        });
        drop(directory);
    }

    #[gpui::test]
    async fn kk_tree_refresh_keeps_previous_rows_and_discards_superseded_results(
        cx: &mut TestAppContext,
    ) {
        cx.background_executor.allow_parking();
        let (directory, repository) = cx
            .background_spawn(async { futures_lite::future::block_on(category_repository()) })
            .await
            .expect("category repository");
        let root = repository.snapshot.repository().root().to_owned();
        cx.update(|cx| {
            let settings = SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            init(cx);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(&root, serde_json::json!({"a.txt": "first category\n"}))
            .await;
        let project = Project::test(fs, [root.as_path()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = multi_workspace.read_with(cx, |view, _| view.workspace().clone());
        let panel = cx
            .update(|window, cx| KkPanel::load(workspace.downgrade(), window.to_async(cx)))
            .await
            .expect("panel");
        let previous = panel.update(cx, |panel, cx| {
            panel.repositories = vec![repository];
            panel.update_tree(cx);
            panel
                .tree_selection
                .rows
                .iter()
                .map(|row| row.id.clone())
                .collect::<Vec<_>>()
        });
        let task = panel.update_in(cx, |panel, window, cx| {
            panel.request_tree(window, cx);
            assert_eq!(
                panel
                    .tree_selection
                    .rows
                    .iter()
                    .map(|row| row.id.clone())
                    .collect::<Vec<_>>(),
                previous
            );
            panel.repositories.clear();
            panel.tree_revision += 1;
            std::mem::replace(&mut panel.tree_task, Task::ready(()))
        });
        task.await;
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel
                    .tree_selection
                    .rows
                    .iter()
                    .map(|row| row.id.clone())
                    .collect::<Vec<_>>(),
                previous
            );
            assert_eq!(panel.tree_requested, None);
        });
        let task = panel.update_in(cx, |panel, window, cx| {
            panel.request_tree(window, cx);
            std::mem::replace(&mut panel.tree_task, Task::ready(()))
        });
        task.await;
        panel.read_with(cx, |panel, _| {
            assert!(panel.tree_selection.rows.is_empty());
        });
        drop(directory);
    }

    #[gpui::test(iterations = 3)]
    async fn tree_navigation_groups_categories_and_preserves_selection_without_stealing_focus(
        cx: &mut TestAppContext,
    ) {
        cx.background_executor.allow_parking();
        let (directory, repository) = cx
            .background_spawn(async { futures_lite::future::block_on(category_repository()) })
            .await
            .expect("prepare category history");
        let root = repository.snapshot.repository().root().to_owned();
        let comparison = repository.comparison.clone().expect("comparison");
        cx.update(|cx| {
            let settings = SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            init(cx);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(&root, serde_json::json!({"a.txt":"first category\n", "b.txt":"second category\n", "c.txt":"current work\n"})).await;
        let project = Project::test(fs, [root.as_path()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = multi_workspace.read_with(cx, |view, _| view.workspace().clone());
        let panel = cx
            .update(|window, cx| KkPanel::load(workspace.downgrade(), window.to_async(cx)))
            .await
            .expect("panel");
        let comparison_id = TreeRowId::Comparison(root.clone());
        let category_id = TreeRowId::Category(root.clone(), "Reviewed");
        panel.update(cx, |panel, cx| {
            panel.repositories = vec![repository];
            panel.mode = HistoryMode::Log;
            panel
                .file_order
                .set(FileOrder::FileName, cx, KkPanel::show_error);
            panel.update_tree(cx);
            assert!(!panel.tree_selection.grouped);
            assert_eq!(
                panel
                    .tree_selection
                    .rows
                    .first()
                    .map(|row| (&row.id, row.depth)),
                Some((&comparison_id, 0))
            );
            assert_eq!(
                panel
                    .tree_selection
                    .rows
                    .iter()
                    .filter(|row| row.id == category_id)
                    .count(),
                1
            );
            assert!(
                !panel
                    .visible_files
                    .iter()
                    .any(|file| file.category == Some(Category::Reviewed))
            );
            panel.select_tree_row(&category_id, false, false, cx);
        });
        panel.update_in(cx, |panel, window, cx| panel.expand_tree(true, window, cx));
        let files = panel.update(cx, |panel, cx| {
            panel.update_tree(cx);
            let files = panel
                .visible_files
                .iter()
                .filter(|file| file.category == Some(Category::Reviewed))
                .cloned()
                .collect::<Vec<_>>();
            assert_eq!(
                files
                    .iter()
                    .map(|file| file.file.path.as_str())
                    .collect::<Vec<_>>(),
                ["a.txt", "b.txt"]
            );
            assert_eq!(files[0].id.section, files[1].id.section);
            assert_ne!(files[0].id.source, files[1].id.source);
            panel.selected_change =
                panel.repositories[0]
                    .history
                    .revisions
                    .first()
                    .map(|revision| {
                        (
                            root.clone(),
                            revision.full_change_id.clone(),
                            revision.full_commit_id.clone(),
                        )
                    });
            panel.select_tree_row(&TreeRowId::File(files[0].id.clone()), false, false, cx);
            let selected = panel.selected_revision().expect("selected file source").1;
            assert_eq!(selected.revset(), files[0].target.identity);
            panel.copy_revision_id(false, cx);
            assert_eq!(
                cx.read_from_clipboard().expect("change ID").text(),
                Some(selected.full_change_id)
            );
            panel.select_tree_row(&TreeRowId::File(files[1].id.clone()), true, false, cx);
            assert_eq!(
                panel
                    .selected_file_rows(&files[1])
                    .expect("same category across commits")
                    .len(),
                2
            );
            panel.select_tree_row(&category_id, false, false, cx);
            panel.select_tree_row(&TreeRowId::File(files[1].id.clone()), true, false, cx);
            assert!(
                panel.selected_file_rows(&files[1]).is_err(),
                "mixed header/file selection must be rejected"
            );
            files
        });
        let editor = cx.new_window_entity(editor::Editor::single_line);
        let focus = editor.read_with(cx, |editor, cx| editor.focus_handle(cx));
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(Box::new(editor), None, true, window, cx)
        });
        let metadata = panel.update(cx, |panel, cx| {
            panel.refresh_commit_emptiness(cx);
            panel
                .load_tasks
                .remove(&format!("empty:{}", root.display()))
                .expect("metadata task")
        });
        metadata.await;
        panel.read_with(cx, |panel, _| {
            let history = &panel.repositories[0].history.revisions;
            let emptiness = panel
                .tree_selection
                .empty_commits
                .get(&root)
                .expect("empty metadata");
            assert_eq!(
                emptiness.get(&history.first().expect("working copy").full_commit_id),
                Some(&false)
            );
            assert_eq!(
                emptiness.get(&history.last().expect("root").full_commit_id),
                Some(&true)
            );
        });
        panel.update(cx, |panel, cx| {
            panel.select_tree_row(&TreeRowId::File(files[1].id.clone()), false, false, cx);
            for category in &mut panel.repositories[0]
                .comparison
                .as_mut()
                .expect("comparison")
                .categories
            {
                category.files.retain(|file| file.path != "b.txt");
            }

            panel.update_tree(cx);
            assert_eq!(
                panel.tree_selection.selected,
                HashSet::from([TreeRowId::File(files[0].id.clone())])
            );
            for category in &mut panel.repositories[0]
                .comparison
                .as_mut()
                .expect("comparison")
                .categories
            {
                category.files.clear();
            }

            panel.update_tree(cx);
            assert_eq!(
                panel.tree_selection.selected,
                HashSet::from([comparison_id.clone()])
            );
            panel.repositories[0].comparison = Some(comparison.clone());
            panel.update_tree(cx);
            panel.select_tree_row(&TreeRowId::File(files[0].id.clone()), false, false, cx);
            panel.select_tree_row(&comparison_id, false, true, cx);
            for category in &mut panel.repositories[0]
                .comparison
                .as_mut()
                .expect("comparison")
                .categories
            {
                category.files.clear();
            }

            panel.update_tree(cx);
            assert_eq!(
                panel.tree_selection.selected,
                HashSet::from([comparison_id.clone()]),
                "keep surviving selections instead of replacing them"
            );
        });
        cx.update(|window, _| {
            assert!(
                focus.is_focused(window),
                "tree reconciliation must preserve editor focus"
            )
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_panel(panel.clone(), window, cx);
            workspace.focus_panel::<KkPanel>(window, cx);
        });
        let previous_comparison = panel.update(cx, |panel, cx| {
            let previous = panel.repositories[0].comparison.replace(comparison.clone());
            panel.update_tree(cx);
            previous
        });
        for id in [
            &comparison_id,
            &category_id,
            &TreeRowId::History(root.clone()),
        ] {
            let before = panel.read_with(cx, |panel, _| panel.tree_selection.is_expanded(id));
            click_disclosure(&panel, id, cx);
            panel.update(cx, |panel, cx| {
                panel.update_tree(cx);
                assert_eq!(
                    panel.tree_selection.is_expanded(id),
                    !before,
                    "arrow must toggle exactly once: {id:?}"
                );
            });
            click_disclosure(&panel, id, cx);
            panel.update(cx, |panel, cx| {
                panel.update_tree(cx);
                assert_eq!(panel.tree_selection.is_expanded(id), before);
            });
        }

        let revision = panel.read_with(cx, |panel, _| {
            panel.repositories[0].history.revisions[0].clone()
        });
        let revision_id = TreeRowId::Revision(root.clone(), revision.full_change_id.clone());
        click_disclosure(&panel, &revision_id, cx);
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let graph_width = cx
            .debug_bounds("kk-history-graph")
            .expect("history graph")
            .size
            .width;
        cx.condition(&panel, |panel, _| {
            matches!(
                panel.repositories[0].expanded.get(&revision.full_commit_id),
                Some(RevisionFiles::Loaded(_))
            )
        })
        .await;
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert_eq!(
            cx.debug_bounds("kk-history-graph")
                .expect("expanded history graph")
                .size
                .width,
            graph_width
        );
        click_disclosure(&panel, &revision_id, cx);
        panel.read_with(cx, |panel, _| {
            assert!(
                !panel.repositories[0]
                    .expanded
                    .contains_key(&revision.full_commit_id)
            );
        });
        panel.update(cx, |panel, cx| {
            panel.tree_selection.grouped = true;
            panel.update_tree(cx);
            cx.notify();
        });
        let repository_id = TreeRowId::Repository(root.clone());
        click_disclosure(&panel, &repository_id, cx);
        panel.update(cx, |panel, cx| {
            panel.update_tree(cx);
            assert!(panel.tree_selection.is_expanded(&repository_id));
        });
        click_disclosure(&panel, &repository_id, cx);
        panel.update(cx, |panel, cx| {
            panel.update_tree(cx);
            assert!(!panel.tree_selection.is_expanded(&repository_id));
            panel.tree_selection.grouped = false;
            panel.repositories[0].comparison = previous_comparison;
            panel.update_tree(cx);
            cx.notify();
        });
        assert!(workspace.read_with(cx, |workspace, cx| {
            workspace.items_of_type::<KkDiff>(cx).next().is_none()
        }));
        panel.update(cx, |panel, cx| panel.navigate_tree(-1, true, false, cx));
        cx.update(|window, cx| {
            window.refresh();
            window.draw(cx).clear(cx);
        });
        cx.simulate_keystrokes("right");
        panel.read_with(cx, |panel, _| {
            assert!(panel.tree_selection.focused_file().is_some());
        });
        cx.simulate_keystrokes("left");
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.tree_selection.cursor, Some(comparison_id.clone()))
        });
        cx.simulate_keystrokes("left");
        panel.update(cx, |panel, cx| {
            panel.update_tree(cx);
            assert!(!panel.tree_selection.is_expanded(&comparison_id));
        });
        cx.simulate_keystrokes("left");
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.tree_selection.cursor, Some(comparison_id.clone()))
        });
        cx.simulate_keystrokes("right");
        cx.simulate_keystrokes("right");
        panel.update(cx, |panel, cx| {
            panel.update_tree(cx);
            assert!(panel.tree_selection.is_expanded(&comparison_id));
            let comparison = panel.repositories[0]
                .comparison
                .as_mut()
                .expect("comparison");
            let original = comparison.files.first().expect("working file").clone();
            comparison.files = (0..2000)
                .map(|index| {
                    let mut file = original.clone();
                    file.path = format!("long-directory/file-{index:04}.txt");
                    file
                })
                .collect();
            panel.update_tree(cx);
            cx.notify();
        });
        cx.update(|window, cx| {
            window.refresh();
            window.draw(cx).clear(cx);
        });
        cx.simulate_keystrokes("end");
        cx.update(|window, cx| {
            window.refresh();
            window.draw(cx).clear(cx);
        });
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.tree_selection.cursor,
                panel.tree_selection.rows.last().map(|row| row.id.clone())
            );
            assert_eq!(panel.scroll_handle.is_scrolled_to_end(), Some(true));
        });
        cx.simulate_keystrokes("shift-home");
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.tree_selection.selected.len(),
                panel.tree_selection.rows.len()
            );
        });
        cx.simulate_keystrokes("down");
        cx.dispatch_action(editor::actions::SelectAll);
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.tree_selection.selected.len(),
                panel.tree_selection.rows.len()
            );
        });
        panel.update(cx, |panel, cx| {
            panel.collapse_tree(cx);
            panel.update_tree(cx);
            assert_eq!(panel.tree_selection.rows.len(), 2);
            assert!(panel.tree_selection.rows.iter().all(|row| row.depth == 0));
            assert_eq!(
                panel.tree_selection.selected,
                HashSet::from([comparison_id.clone(), TreeRowId::History(root.clone())])
            );
        });
        drop(directory);
    }

    #[gpui::test(iterations = 3)]
    async fn repository_tree_actions_choose_a_root_and_keep_other_repositories_unchanged(
        cx: &mut TestAppContext,
    ) {
        cx.background_executor.allow_parking();
        let ((first_directory, first), (second_directory, second)) = cx
            .background_spawn(async {
                futures_lite::future::block_on(async {
                    anyhow::Ok((category_repository().await?, category_repository().await?))
                })
            })
            .await
            .expect("two independent repositories");
        let first_root = first.snapshot.repository().root().to_owned();
        let second_root = second.snapshot.repository().root().to_owned();
        cx.update(|cx| {
            let settings = SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            init(cx);
        });
        let fs = FakeFs::new(cx.executor());
        for root in [&first_root, &second_root] {
            fs.insert_tree(root, serde_json::json!({"a.txt":"first category\n", "b.txt":"second category\n", "c.txt":"current work\n"})).await;
        }

        let project = Project::test(fs, [first_root.as_path(), second_root.as_path()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = multi_workspace.read_with(cx, |view, _| view.workspace().clone());
        let panel = cx
            .update(|window, cx| KkPanel::load(workspace.downgrade(), window.to_async(cx)))
            .await
            .expect("panel");
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_panel(panel.clone(), window, cx);
            workspace.focus_panel::<KkPanel>(window, cx);
        });
        panel.update_in(cx, |panel, window, cx| {
            panel.repositories = vec![first, second];
            panel.update_tree(cx);
            assert_eq!(panel.tree_selection.rows.len(), 2);
            assert!(
                panel
                    .tree_selection
                    .rows
                    .iter()
                    .all(|row| row.expanded == Some(false))
            );
            for root in [&first_root, &second_root] {
                panel
                    .tree_selection
                    .expanded
                    .insert(TreeRowId::Repository(root.clone()), true);
            }

            panel.update_tree(cx);
            panel.close_comparison(&CloseComparison, window, cx);
            assert!(
                panel
                    .repositories
                    .iter()
                    .all(|repository| repository.comparison.is_some())
            );
        });
        cx.run_until_parked();
        let chooser = workspace
            .read_with(cx, |workspace, cx| {
                workspace.active_modal::<crate::picker_prompt::PickerPrompt>(cx)
            })
            .expect("multiple roots need a chooser");
        let picker = chooser.read_with(cx, |chooser, _| chooser.picker.clone());
        picker.update_in(cx, |picker, window, cx| {
            picker.update_matches(second_root.display().to_string(), window, cx);
        });
        cx.run_until_parked();
        cx.dispatch_action(menu::Confirm);
        cx.run_until_parked();
        panel.update_in(cx, |panel, window, cx| {
            assert!(panel.repositories[0].comparison.is_some());
            assert!(panel.repositories[1].comparison.is_none());
            panel.select_tree_row(&TreeRowId::History(second_root.clone()), false, false, cx);
            panel.repositories[1].history.has_more = true;
            panel.choose_repository(window, cx, |panel, index, _, cx| {
                assert_eq!(index, 1);
                panel.load_more_commits(index, cx);
            });
            assert_eq!(panel.history_limits.get(&second_root), Some(&100));
            assert!(!panel.history_limits.contains_key(&first_root));
            panel.load_more_commits(1, cx);
            assert_eq!(
                panel.history_limits.get(&second_root),
                Some(&100),
                "repeated clicks share the pending page"
            );
        });
        cx.condition(&panel, |panel, _| !panel.loading).await;
        panel.update(cx, |panel, cx| {
            assert!(panel.history_loads.is_empty());
            let index = panel
                .repositories
                .iter()
                .position(|repository| repository.snapshot.repository().root() == second_root)
                .expect("second root");
            panel.repositories[index].history.has_more = true;
            panel
                .history_loads
                .insert(second_root.clone(), HistoryLoad::Failed);
            panel.load_more_commits(index, cx);
            assert_eq!(
                panel.history_limits.get(&second_root),
                Some(&100),
                "retry must keep the failed page limit"
            );
        });
        cx.condition(&panel, |panel, _| !panel.loading).await;
        panel.update_in(cx, |panel, _window, cx| {
            assert_eq!(panel.history_limits.get(&first_root), Some(&50));
            assert_eq!(panel.history_limits.get(&second_root), Some(&100));
            assert!(
                panel
                    .repositories
                    .iter()
                    .find(|repository| repository.snapshot.repository().root() == first_root)
                    .expect("first root")
                    .comparison
                    .is_some()
            );
            assert!(
                panel
                    .repositories
                    .iter()
                    .find(|repository| repository.snapshot.repository().root() == second_root)
                    .expect("second root")
                    .comparison
                    .is_none()
            );
            panel.select_tree_row(&TreeRowId::History(second_root.clone()), false, false, cx);
        });
        click_row_icon(
            &panel,
            &TreeRowId::Comparison(first_root.clone()),
            "ICON-Close",
            cx,
        );
        panel.read_with(cx, |panel, _| {
            assert!(
                panel
                    .repositories
                    .iter()
                    .all(|repository| repository.comparison.is_none()),
                "inline Close uses its row even when another repository is selected"
            );
        });
        let (compared, working_copy) = panel.read_with(cx, |panel, _| {
            let repository = panel
                .repositories
                .iter()
                .find(|repository| repository.snapshot.repository().root() == second_root)
                .expect("second repository");
            (
                repository
                    .history
                    .revisions
                    .iter()
                    .find(|revision| {
                        !revision.is_working_copy
                            && !revision.is_root
                            && Category::from_revision(revision).is_none()
                    })
                    .expect("comparison base")
                    .clone(),
                repository
                    .history
                    .revisions
                    .iter()
                    .find(|revision| revision.is_working_copy)
                    .expect("working copy")
                    .clone(),
            )
        });
        right_click_revision(&panel, &second_root, &compared, cx);
        let compare_entry = cx
            .debug_bounds("MENU_ITEM-Compare with Change")
            .expect("historical revision menu offers Compare With");
        cx.simulate_click(compare_entry.center(), Modifiers::none());
        let comparison = panel.update(cx, |panel, _| {
            assert!(panel.context_menu.is_none());
            let repository = panel
                .repositories
                .iter()
                .find(|repository| repository.snapshot.repository().root() == second_root)
                .expect("clicked repository");
            assert_eq!(
                repository
                    .comparison
                    .as_ref()
                    .expect("right-click Compare With starts comparison")
                    .intent
                    .target_revset,
                compared.revset()
            );
            panel
                .load_tasks
                .remove(&format!("compare:{}", second_root.display()))
        });
        if let Some(comparison) = comparison {
            comparison.await;
        }
        let comparison_target = panel.read_with(cx, |panel, _| {
            let first = panel
                .repositories
                .iter()
                .find(|repository| repository.snapshot.repository().root() == first_root)
                .expect("first repository");
            let second = panel
                .repositories
                .iter()
                .find(|repository| repository.snapshot.repository().root() == second_root)
                .expect("second repository");
            assert!(
                first.comparison.is_none(),
                "compare the clicked repository only"
            );
            let comparison = second.comparison.as_ref().expect("comparison");
            assert_eq!(comparison.intent.target_revset, compared.revset());
            assert!(comparison.files.iter().any(|file| file.path == "c.txt"));
            comparison.intent.target_revset.clone()
        });
        right_click_revision(&panel, &second_root, &working_copy, cx);
        assert!(cx.debug_bounds("MENU_ITEM-Compare with Change").is_none());
        let copy_entry = cx
            .debug_bounds("MENU_ITEM-Copy Change ID")
            .expect("working-copy copy entry");
        cx.simulate_click(copy_entry.center(), Modifiers::none());
        panel.read_with(cx, |panel, cx| {
            assert_eq!(
                cx.read_from_clipboard().and_then(|item| item.text()),
                Some(working_copy.full_change_id.clone()),
                "working-copy menu starts with Copy Change ID"
            );
            let second = panel
                .repositories
                .iter()
                .find(|repository| repository.snapshot.repository().root() == second_root)
                .expect("second repository");
            assert_eq!(
                second
                    .comparison
                    .as_ref()
                    .expect("existing comparison")
                    .intent
                    .target_revset,
                comparison_target
            );
            assert!(
                !panel
                    .load_tasks
                    .contains_key(&format!("compare:{}", second_root.display()))
            );
        });
        panel.update(cx, |panel, cx| {
            let repositories = panel.repositories.clone();
            panel.repositories.truncate(1);
            panel.update_tree(cx);
            assert!(
                panel.tree_selection.grouped,
                "keep repository grouping after removing a root"
            );
            panel.repositories.clear();
            panel.update_tree(cx);
            assert!(!panel.tree_selection.grouped);
            panel.repositories = repositories.iter().take(1).cloned().collect();
            panel.update_tree(cx);
            assert_eq!(
                panel
                    .tree_selection
                    .rows
                    .first()
                    .map(|row| (&row.id, row.depth)),
                Some((&TreeRowId::History(first_root.clone()), 0))
            );
            panel.repositories = repositories;
            panel.update_tree(cx);
        });
        let metadata = panel.update(cx, |panel, cx| {
            panel.tree_selection.empty_commits.clear();
            panel.refresh_commit_emptiness(cx);
            panel.repositories.clear();
            panel.tree_selection.forget_root(&first_root);
            panel.tree_selection.forget_root(&second_root);
            [&first_root, &second_root]
                .into_iter()
                .filter_map(|root| {
                    panel
                        .load_tasks
                        .remove(&format!("empty:{}", root.display()))
                })
                .collect::<Vec<_>>()
        });
        assert_eq!(metadata.len(), 2);
        for task in metadata {
            task.await;
        }

        panel.read_with(cx, |panel, _| {
            assert!(
                panel.tree_selection.empty_commits.is_empty(),
                "metadata from removed repositories must not return"
            );
        });
        drop((first_directory, second_directory));
    }

    #[gpui::test(iterations = 3)]
    async fn kk_remote_stale_recovery_confirms_once_and_respects_root_closure(
        cx: &mut TestAppContext,
        server_cx: &mut TestAppContext,
    ) {
        run_stale_recovery(cx, server_cx)
            .await
            .expect("stale workspace recovery");
    }

    async fn run_stale_recovery(
        cx: &mut TestAppContext,
        server_cx: &mut TestAppContext,
    ) -> anyhow::Result<()> {
        use crate::kk_panel::remote_tests::{RemoteFixture, run_kk};
        use anyhow::Context as _;
        for initially_stale in [false, true] {
            let fixture = RemoteFixture::create(cx, server_cx).await?;
            let project = fixture.project.clone();
            run_kk(
                &fixture.root,
                &[
                    "config",
                    "set",
                    "--repo",
                    "snapshot.auto-update-stale",
                    "false",
                ],
            )
            .await?;
            run_kk(&fixture.root, &["status"]).await?;
            if initially_stale {
                run_kk(&fixture.root, &["new", "root()", "--ignore-working-copy"]).await?;
            }

            let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
                MultiWorkspace::test_new(project.clone(), window, cx)
            });
            let workspace = multi_workspace.read_with(cx, |view, _| view.workspace().clone());
            let panel = cx
                .update(|window, cx| KkPanel::load(workspace.downgrade(), window.to_async(cx)))
                .await?;
            workspace.update_in(cx, |workspace, window, cx| {
                workspace.add_panel(panel.clone(), window, cx)
            });
            panel
                .update(cx, |panel, cx| panel.refresh_for_test(cx))
                .await;
            if !initially_stale {
                panel.update(cx, |panel, cx| {
                    panel.update_tree(cx);
                    assert!(!panel.tree_selection.grouped);
                });
                run_kk(&fixture.root, &["new", "root()", "--ignore-working-copy"]).await?;
                panel
                    .update(cx, |panel, cx| panel.refresh_for_test(cx))
                    .await;
            }

            let recovery = if initially_stale {
                TreeRowId::Repository(fixture.root.clone())
            } else {
                TreeRowId::History(fixture.root.clone())
            };
            panel.update(cx, |panel, cx| {
                panel.update_tree(cx);
                assert!(panel.error.is_none(), "{:?}", panel.error);
                assert_eq!(panel.repositories.len(), 1);
                assert!(panel.repositories[0].stale);
                assert_eq!(panel.tree_selection.grouped, initially_stale);
                assert_eq!(panel.tree_selection.rows.len(), 1);
                assert!(matches!(
                    panel.tree_selection.rows[0].kind,
                    TreeRowKind::Recovery(_)
                ));
                panel.select_tree_row(&recovery, false, false, cx);
            });
            panel.update_in(cx, |panel, window, cx| {
                panel.confirm_tree(DiffOpenMode::Open, window, cx);
                panel.confirm_tree(DiffOpenMode::Open, window, cx);
                assert_eq!(panel.updating_stale.len(), 1);
            });
            assert!(cx.has_pending_prompt());
            cx.simulate_prompt_answer("Cancel");
            cx.condition(&panel, |panel, _| panel.updating_stale.is_empty())
                .await;
            assert_eq!(std::fs::read(fixture.root.join("file.txt"))?, b"captured\n");
            let path = project
                .read_with(cx, |project, cx| {
                    project.find_project_path(&fixture.root.join("file.txt"), cx)
                })
                .context("working file path")?;
            let buffer = project
                .update(cx, |project, cx| project.open_buffer(path, cx))
                .await?;
            buffer.update(cx, |buffer, cx| {
                buffer.edit([(0..0, "unsaved draft\n")], None, cx);
            });
            panel.update_in(cx, |panel, window, cx| {
                panel.confirm_tree(DiffOpenMode::Open, window, cx)
            });
            cx.simulate_prompt_answer("Update Workspace");
            cx.condition(&panel, |panel, _| {
                panel.updating_stale.is_empty() && !panel.loading
            })
            .await;
            panel.update(cx, |panel, cx| {
                panel.update_tree(cx);
                assert!(!panel.repositories[0].stale);
                assert!(
                    panel.operation_error.is_none(),
                    "{:?}",
                    panel.operation_error
                );
                assert_eq!(panel.pending_writes, 0);
                assert!(
                    !panel
                        .tree_selection
                        .rows
                        .iter()
                        .any(|row| matches!(row.kind, TreeRowKind::Recovery(_)))
                );
            });
            assert!(!fixture.root.join("file.txt").exists());
            buffer.read_with(cx, |buffer, _| {
                assert_eq!(buffer.text(), "unsaved draft\ncaptured\n");
                assert!(buffer.is_dirty());
            });
            std::fs::write(fixture.root.join("file.txt"), "preserve on close\n")?;
            run_kk(&fixture.root, &["status"]).await?;
            run_kk(&fixture.root, &["new", "root()", "--ignore-working-copy"]).await?;
            panel
                .update(cx, |panel, cx| panel.refresh_for_test(cx))
                .await;
            panel.update_in(cx, |panel, window, cx| {
                panel.update_tree(cx);
                panel.select_tree_row(&recovery, false, false, cx);
                panel.confirm_tree(DiffOpenMode::Open, window, cx);
            });
            let roots = project.read_with(cx, |project, cx| {
                project
                    .visible_worktrees(cx)
                    .map(|tree| tree.read(cx).id())
                    .collect::<Vec<_>>()
            });
            project.update(cx, |project, cx| {
                for root in roots {
                    project.remove_worktree(root, cx);
                }
            });
            cx.run_until_parked();
            cx.simulate_prompt_answer("Update Workspace");
            cx.run_until_parked();
            assert_eq!(
                std::fs::read(fixture.root.join("file.txt"))?,
                b"preserve on close\n"
            );
        }

        Ok(())
    }
}

struct TreeProjection {
    repositories: Vec<RepositoryView>,
    repository_paths: HashMap<PathBuf, Vec<(EntityId, PathBuf)>>,
    tree_selection: TreeSelectionState,
    list_previews: ListPreviews,
    mutation_previews: MutationPreviews,
    file_order: super::super::kk_file_order::FileSort,
    mode: HistoryMode,
}

struct TreePresentation {
    tree_rows: Vec<TreeRow>,
    rendered: Vec<TreeRenderRow>,
    visible_files: Vec<FileRow>,
    graph_widths: HashMap<PathBuf, usize>,
}

impl TreeProjection {
    fn build(&self) -> TreePresentation {
        let mut tree_rows = self.tree_rows();
        let mut rendered = Vec::new();
        let mut visible_files = Vec::new();
        let mut graph_widths = HashMap::new();
        for (index, row) in tree_rows.iter_mut().enumerate() {
            row.render_index = rendered.len();
            rendered.push(TreeRenderRow::Entry(index));
            if let TreeRowKind::Revision(_, graph) = &row.kind {
                let width = graph
                    .incoming
                    .iter()
                    .chain(&graph.outgoing)
                    .copied()
                    .chain([graph.column])
                    .max()
                    .unwrap_or(0)
                    + 1;
                let previous = graph_widths
                    .entry(row.id.root().to_owned())
                    .or_insert(1usize);
                *previous = (*previous).max(width);
            }
            if let TreeRowKind::Revision(revision, _) = &row.kind
                && let Some(repository) = self
                    .repositories
                    .iter()
                    .find(|repository| repository.snapshot.repository().root() == row.id.root())
            {
                match repository.expanded.get(&revision.full_commit_id) {
                    Some(RevisionFiles::Failed(error)) => {
                        rendered.push(TreeRenderRow::Error {
                            root: row.id.root().to_owned(),
                            revision: revision.clone(),
                            depth: row.depth + 1,
                            error: error.clone(),
                        });
                    }
                    Some(RevisionFiles::Loading) => {
                        rendered.push(TreeRenderRow::Message {
                            depth: row.depth + 1,
                            text: "Loading…",
                        });
                    }
                    Some(RevisionFiles::Loaded(files)) if files.is_empty() => {
                        rendered.push(TreeRenderRow::Message {
                            depth: row.depth + 1,
                            text: "No file changes",
                        });
                    }
                    _ => {}
                }
            }

            if let TreeRowKind::File(file) = &row.kind {
                visible_files.push(file.clone());
            }
        }

        TreePresentation {
            tree_rows,
            rendered,
            visible_files,
            graph_widths,
        }
    }

    fn tree_rows(&self) -> Vec<TreeRow> {
        let mut rows = Vec::new();
        let depth = usize::from(self.tree_selection.grouped);
        for repository in &self.repositories {
            let root = repository.snapshot.repository().root().to_owned();
            let repository_id = TreeRowId::Repository(root.clone());
            let expanded = self.tree_selection.is_expanded(&repository_id);
            if self.tree_selection.grouped {
                let label = self
                    .repository_paths
                    .get(&root)
                    .and_then(|paths| {
                        paths.iter().find_map(|(_, path)| {
                            root.strip_prefix(path)
                                .ok()
                                .filter(|relative| !relative.as_os_str().is_empty())
                        })
                    })
                    .map(|relative| relative.display().to_string())
                    .unwrap_or_else(|| {
                        root.file_name()
                            .unwrap_or(root.as_os_str())
                            .to_string_lossy()
                            .into_owned()
                    });
                rows.push(TreeRow {
                    id: repository_id.clone(),
                    parent: None,
                    fallback: None,
                    depth: 0,
                    expanded: (!repository.stale).then_some(expanded),
                    kind: if repository.stale {
                        TreeRowKind::Recovery(label)
                    } else {
                        TreeRowKind::Group(label)
                    },
                    render_index: 0,
                });
                if !expanded || repository.stale {
                    continue;
                }
            }

            let parent = self.tree_selection.grouped.then_some(repository_id);
            let comparison_id = TreeRowId::Comparison(root.clone());
            if let Some(comparison) = &repository.comparison {
                let mut projected = self
                    .list_previews
                    .files_with_mutations(repository, &self.mutation_previews);
                if self.file_order.order() == FileOrder::FileName {
                    projected.sort_by(|left, right| {
                        self.file_order.compare(&left.file.path, &right.file.path)
                    });
                }

                let expanded = self.tree_selection.is_expanded(&comparison_id);
                rows.push(TreeRow {
                    id: comparison_id.clone(),
                    parent: parent.clone(),
                    fallback: None,
                    depth,
                    expanded: Some(expanded),
                    kind: TreeRowKind::Group(format!(
                        "Comparison: {} ({})",
                        comparison.intent.target_label,
                        projected
                            .iter()
                            .filter(|file| file.category.is_none())
                            .count()
                    )),
                    render_index: 0,
                });
                if expanded && !repository.stale {
                    for file in projected
                        .iter()
                        .filter(|file| file.category.is_none())
                        .cloned()
                    {
                        rows.push(TreeRow {
                            id: TreeRowId::File(file.id.clone()),
                            parent: Some(comparison_id.clone()),
                            fallback: None,
                            depth: depth + 1,
                            expanded: None,
                            kind: TreeRowKind::File(file),
                            render_index: 0,
                        });
                    }
                }

                for category in [
                    Category::Rejected,
                    Category::Debug,
                    Category::Pending,
                    Category::Reviewed,
                ] {
                    let files = projected
                        .iter()
                        .filter(|file| file.category == Some(category))
                        .cloned()
                        .collect::<Vec<_>>();
                    if files.is_empty() {
                        continue;
                    }

                    let id = TreeRowId::Category(root.clone(), category.label());
                    let expanded = self.tree_selection.is_expanded(&id);
                    rows.push(TreeRow {
                        id: id.clone(),
                        parent: parent.clone(),
                        fallback: Some(comparison_id.clone()),
                        depth,
                        expanded: Some(expanded),
                        kind: TreeRowKind::Group(format!("{} ({})", category.label(), files.len())),
                        render_index: 0,
                    });
                    if expanded && !repository.stale {
                        for file in files {
                            rows.push(TreeRow {
                                id: TreeRowId::File(file.id.clone()),
                                parent: Some(id.clone()),
                                fallback: None,
                                depth: depth + 1,
                                expanded: None,
                                kind: TreeRowKind::File(file),
                                render_index: 0,
                            });
                        }
                    }
                }
            }

            let history_id = TreeRowId::History(root.clone());
            let expanded = self.tree_selection.is_expanded(&history_id);
            rows.push(TreeRow {
                id: history_id.clone(),
                parent,
                fallback: None,
                depth,
                expanded: (!repository.stale).then_some(expanded),
                kind: if repository.stale {
                    TreeRowKind::Recovery("Commits".into())
                } else {
                    TreeRowKind::Group("Commits".into())
                },
                render_index: 0,
            });
            if !expanded || repository.stale {
                continue;
            }

            let graph = history_graph(&repository.history.revisions, self.mode);
            let revisions = repository
                .located
                .iter()
                .chain(repository.history.revisions.iter());
            let prefixes = repository
                .located
                .iter()
                .map(|revision| kk_workbench::GraphRow {
                    prefix: "◇".to_owned(),
                    column: 0,
                    incoming: Vec::new(),
                    outgoing: Vec::new(),
                    parents: Vec::new(),
                    is_fork: revision.has_multiple_children,
                    is_merge: revision.parent_commit_ids.len() > 1,
                    omitted_parents: 0,
                })
                .chain(graph);
            let mut change_counts = HashMap::new();
            for revision in repository
                .history
                .revisions
                .iter()
                .chain(repository.located.iter())
            {
                *change_counts
                    .entry(&revision.full_change_id)
                    .or_insert(0usize) += 1;
            }
            for (revision, prefix) in revisions.zip(prefixes) {
                let identity = if change_counts
                    .get(&revision.full_change_id)
                    .copied()
                    .unwrap_or(0)
                    > 1
                {
                    format!("{}:{}", revision.full_change_id, revision.full_commit_id)
                } else {
                    revision.full_change_id.clone()
                };
                let id = TreeRowId::Revision(root.clone(), identity);
                let expanded = repository.expanded.get(&revision.full_commit_id);
                rows.push(TreeRow {
                    id: id.clone(),
                    parent: Some(history_id.clone()),
                    fallback: None,
                    depth: depth + 1,
                    expanded: Some(expanded.is_some()),
                    kind: TreeRowKind::Revision(revision.clone(), prefix),
                    render_index: 0,
                });
                if let Some(RevisionFiles::Loaded(files)) = expanded {
                    let target = super::super::kk_diff::DiffTarget::change(revision);
                    let mut projected = Vec::new();
                    for file in files {
                        projected.push(FileRow::new(
                            repository,
                            super::super::kk_diff::DiffTarget::change(revision),
                            file.clone(),
                            "commit",
                            revision.is_working_copy,
                        ));
                    }
                    self.mutation_previews
                        .project(repository, Some(&target), &mut projected);
                    projected
                        .retain(|row| !self.list_previews.hides(repository, revision, &row.file));
                    if self.file_order.order() == FileOrder::FileName {
                        projected.sort_by(|left, right| {
                            self.file_order.compare(&left.file.path, &right.file.path)
                        });
                    }

                    for file in projected {
                        rows.push(TreeRow {
                            id: TreeRowId::File(file.id.clone()),
                            parent: Some(id.clone()),
                            fallback: None,
                            depth: depth + 2,
                            expanded: None,
                            kind: TreeRowKind::File(file),
                            render_index: 0,
                        });
                    }
                }
            }

            if repository.history.has_more {
                rows.push(TreeRow {
                    id: TreeRowId::More(root),
                    parent: Some(history_id),
                    fallback: None,
                    depth: depth + 1,
                    expanded: None,
                    kind: TreeRowKind::More,
                    render_index: 0,
                });
            }
        }

        rows
    }
}
