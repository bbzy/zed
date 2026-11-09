use super::{file_actions::FileRow, *};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

#[derive(Clone, Default)]
pub(super) struct MutationPreviews {
    next_id: u64,
    intents: Vec<MutationIntent>,
}

#[derive(Clone)]
struct MutationIntent {
    id: u64,
    snapshot: Snapshot,
    effects: Vec<MutationEffect>,
    state: IntentState,
    label: &'static str,
    succeeded: WriteDependency,
}

#[derive(Clone)]
struct MutationEffect {
    target: super::super::kk_diff::DiffTarget,
    file: ChangedFile,
    remaining: Option<ChangedFile>,
}

impl MutationPreviews {
    pub fn file(&mut self, repository: &RepositoryView, request: &FileMutationRequest) -> u64 {
        let mut effects = vec![MutationEffect {
            target: request.target.clone(),
            file: request.file.clone(),
            remaining: None,
        }];
        let revisions =
            repository
                .history
                .revisions
                .iter()
                .chain(repository.located.iter())
                .chain(repository.comparison.iter().flat_map(|comparison| {
                    comparison.categories.iter().map(|entry| &entry.revision)
                }))
                .collect::<Vec<_>>();
        let source = revisions.iter().copied().find(|revision| matches!(&request.target.scope, DiffScope::Change(commit) if *commit == revision.full_commit_id));
        match request.action {
            FileMutationAction::Move(direction) => {
                let destination = source.and_then(|source| match direction {
                    FileMoveDirection::Parent => revisions.iter().copied().find(|revision| {
                        source.parent_commit_ids.as_slice() == [revision.full_commit_id.clone()]
                    }),
                    FileMoveDirection::Child => revisions.iter().copied().find(|revision| {
                        revision.parent_commit_ids.contains(&source.full_commit_id)
                    }),
                });
                if let Some(destination) = destination {
                    effects.push(MutationEffect {
                        target: super::super::kk_diff::DiffTarget::change(destination),
                        file: request.file.clone(),
                        remaining: Some(request.file.clone()),
                    });
                    if source.is_some_and(|source| source.is_working_copy)
                        && let Some(comparison) = &repository.comparison
                        && comparison.intent.target_revset == destination.revset()
                    {
                        effects.push(MutationEffect {
                            target: comparison.diff_target(),
                            file: request.file.clone(),
                            remaining: None,
                        });
                    }
                }
            }
            FileMutationAction::Revert => {
                if matches!(request.target.scope, DiffScope::Comparison { .. }) {
                    if let Some(working) =
                        revisions.iter().find(|revision| revision.is_working_copy)
                    {
                        effects.push(MutationEffect {
                            target: super::super::kk_diff::DiffTarget::change(working),
                            file: request.file.clone(),
                            remaining: Some(request.file.clone()),
                        });
                    }
                } else if let Some(comparison) = &repository.comparison {
                    effects.push(MutationEffect {
                        target: comparison.diff_target(),
                        file: request.file.clone(),
                        remaining: Some(request.file.clone()),
                    });
                }
            }
        }

        self.push(request.snapshot.clone(), effects, request.action.label())
    }

    pub fn conflict(&mut self, repository: &RepositoryView, request: &ConflictRequest) -> u64 {
        let mut effects = Vec::new();
        let revision =
            repository
                .history
                .revisions
                .iter()
                .chain(repository.located.iter())
                .chain(repository.comparison.iter().flat_map(|comparison| {
                    comparison.categories.iter().map(|entry| &entry.revision)
                }))
                .find(|revision| revision.full_commit_id == request.conflict.commit_id);
        if let Some(revision) = revision {
            let file = repository
                .expanded
                .get(&revision.full_commit_id)
                .and_then(|files| match files {
                    RevisionFiles::Loaded(files) => files
                        .iter()
                        .find(|file| file.path == request.conflict.path)
                        .cloned(),
                    _ => None,
                })
                .or_else(|| {
                    repository
                        .comparison
                        .as_ref()?
                        .categories
                        .iter()
                        .find(|entry| entry.revision.full_commit_id == revision.full_commit_id)?
                        .files
                        .iter()
                        .find(|file| file.path == request.conflict.path)
                        .cloned()
                })
                .unwrap_or(ChangedFile {
                    path: request.conflict.path.clone(),
                    change_type: kk_workbench::ChangeKind::Modified,
                    renamed_from: None,
                    has_conflict: true,
                });
            let remaining = request.result.as_ref().map(|_| ChangedFile {
                has_conflict: false,
                ..file.clone()
            });
            effects.push(MutationEffect {
                target: super::super::kk_diff::DiffTarget::change(revision),
                file: file.clone(),
                remaining: remaining.clone(),
            });
            if revision.is_working_copy
                && let Some(comparison) = &repository.comparison
            {
                effects.push(MutationEffect {
                    target: comparison.diff_target(),
                    file,
                    remaining,
                });
            }
        }

        self.push(request.snapshot.clone(), effects, "Applying resolution")
    }

    fn push(
        &mut self,
        snapshot: Snapshot,
        effects: Vec<MutationEffect>,
        label: &'static str,
    ) -> u64 {
        if let Some(existing) = self.intents.iter().find(|intent| {
            intent.state == IntentState::Pending
                && intent.snapshot.repository().root() == snapshot.repository().root()
                && intent.snapshot.repository().lifetime() == snapshot.repository().lifetime()
                && intent.snapshot.operation() == snapshot.operation()
                && intent.label == label
                && intent.effects.len() == effects.len()
                && intent.effects.iter().zip(&effects).all(|(left, right)| {
                    left.target.scope == right.target.scope
                        && left.file == right.file
                        && left.remaining == right.remaining
                })
        }) {
            return existing.id;
        }

        self.next_id += 1;
        let id = self.next_id;
        self.intents.push(MutationIntent {
            id,
            snapshot,
            effects,
            state: IntentState::Pending,
            label,
            succeeded: Arc::new(AtomicBool::new(false)),
        });
        id
    }

    pub fn complete(&mut self, ids: &[u64], outcome: &Result<Snapshot, ClassificationFailure>) {
        self.intents.retain_mut(|intent| {
            if !ids.contains(&intent.id) {
                return true;
            }
            intent.succeeded.store(outcome.is_ok(), Ordering::Release);
            match outcome {
                Ok(_) => intent.state = IntentState::Confirmed,
                Err(error) if error.uncertain => intent.state = IntentState::Uncertain,
                Err(_) => return false,
            }
            true
        });
    }

    pub fn confirmed(&self) -> HashSet<u64> {
        self.intents
            .iter()
            .filter(|intent| intent.state == IntentState::Confirmed)
            .map(|intent| intent.id)
            .collect()
    }

    pub fn refreshed(
        &mut self,
        confirmed: &HashSet<u64>,
        repositories: &[RepositoryView],
        loaded_comparisons: &HashSet<PathBuf>,
    ) {
        self.intents.retain(|intent| {
            !confirmed.contains(&intent.id)
                || !repositories.iter().any(|repository| {
                    repository.snapshot.repository().root() == intent.snapshot.repository().root()
                        && repository.snapshot.repository().lifetime()
                            == intent.snapshot.repository().lifetime()
                        && (repository.comparison.is_none()
                            || loaded_comparisons.contains(repository.snapshot.repository().root()))
                        && repository
                            .expanded
                            .values()
                            .all(|files| matches!(files, RevisionFiles::Loaded(_)))
                })
        });
    }

    pub fn settle_uncertain(&mut self, root: &Path, applied: bool, refreshed: bool) {
        self.intents.retain_mut(|intent| {
            if intent.snapshot.repository().root() != root || intent.state != IntentState::Uncertain
            {
                return true;
            }
            intent.state = IntentState::Confirmed;
            intent.succeeded.store(applied, Ordering::Release);
            applied && !refreshed
        });
    }

    pub fn forget_root(&mut self, root: &Path) {
        self.intents
            .retain(|intent| intent.snapshot.repository().root() != root);
    }

    pub fn project(
        &self,
        repository: &RepositoryView,
        target: Option<&super::super::kk_diff::DiffTarget>,
        rows: &mut Vec<FileRow>,
    ) {
        for intent in self.intents.iter().filter(|intent| {
            intent.snapshot.repository().root() == repository.snapshot.repository().root()
                && intent.snapshot.repository().lifetime()
                    == repository.snapshot.repository().lifetime()
        }) {
            for effect in &intent.effects {
                let mut found = false;
                rows.retain_mut(|row| {
                    if row.target.scope != effect.target.scope
                        || (row.file.path != effect.file.path
                            && effect.file.renamed_from.as_ref() != Some(&row.file.path))
                    {
                        return true;
                    }
                    found = true;
                    if let Some(remaining) = &effect.remaining {
                        row.file.has_conflict = remaining.has_conflict;
                        row.pending = true;
                        row.pending_action = Some(intent.label);
                        row.classification_dependency = Some(intent.succeeded.clone());
                        row.capture_saved = false;
                        return true;
                    }
                    false
                });
                if !found && let Some(remaining) = &effect.remaining {
                    let revision = repository
                        .history
                        .revisions
                        .iter()
                        .chain(repository.located.iter())
                        .chain(repository.comparison.iter().flat_map(|comparison| {
                            comparison.categories.iter().map(|entry| &entry.revision)
                        }))
                        .find(|revision| {
                            effect.target.scope
                                == DiffScope::Change(revision.full_commit_id.clone())
                        });
                    let section = if let Some(target) = target {
                        (target.scope == effect.target.scope).then_some("commit")
                    } else {
                        repository.comparison.as_ref().and_then(|comparison| {
                            if comparison.diff_target().scope == effect.target.scope {
                                Some("comparison")
                            } else if comparison.categories.iter().any(|entry| {
                                effect.target.scope
                                    == DiffScope::Change(entry.revision.full_commit_id.clone())
                            }) {
                                Some("category")
                            } else {
                                None
                            }
                        })
                    };
                    if let Some(section) = section {
                        let mut row = FileRow::new(
                            repository,
                            effect.target.clone(),
                            remaining.clone(),
                            section,
                            revision.is_some_and(|revision| revision.is_working_copy)
                                || section == "comparison",
                        );
                        row.pending = true;
                        row.pending_action = Some(intent.label);
                        row.classification_dependency = Some(intent.succeeded.clone());
                        row.capture_saved = false;
                        rows.push(row);
                    }
                }
            }
        }
    }
}

#[derive(Clone, Default)]
pub(super) struct ListPreviews {
    next_id: u64,
    intents: Vec<ListIntent>,
}

#[derive(Clone)]
struct ListIntent {
    id: u64,
    snapshot: Snapshot,
    comparison: String,
    source: super::super::kk_diff::DiffTarget,
    source_category: Option<ClassificationTarget>,
    files: Vec<FileSelection>,
    destination: ClassificationTarget,
    state: IntentState,
    succeeded: WriteDependency,
    capture_saved: bool,
}

#[derive(Clone, PartialEq)]
enum IntentState {
    Pending,
    Confirmed,
    Uncertain,
}

impl ListPreviews {
    #[cfg(test)]
    pub fn push(
        &mut self,
        snapshot: Snapshot,
        comparison: &ComparisonIntent,
        source: super::super::kk_diff::DiffTarget,
        files: Vec<FileSelection>,
        destination: ClassificationTarget,
    ) -> u64 {
        self.push_from(snapshot, comparison, source, files, destination, None, true)
    }

    pub fn push_from(
        &mut self,
        snapshot: Snapshot,
        comparison: &ComparisonIntent,
        source: super::super::kk_diff::DiffTarget,
        files: Vec<FileSelection>,
        destination: ClassificationTarget,
        source_category: Option<ClassificationTarget>,
        capture_saved: bool,
    ) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        self.intents.push(ListIntent {
            id,
            snapshot,
            comparison: comparison.target_revset.clone(),
            source,
            source_category,
            files,
            destination,
            state: IntentState::Pending,
            succeeded: Arc::new(AtomicBool::new(false)),
            capture_saved,
        });
        id
    }

    pub fn complete(&mut self, ids: &[u64], outcome: &Result<Snapshot, ClassificationFailure>) {
        self.intents.retain_mut(|intent| {
            if !ids.contains(&intent.id) {
                return true;
            }
            intent.succeeded.store(outcome.is_ok(), Ordering::Release);

            match outcome {
                Ok(_) => intent.state = IntentState::Confirmed,
                Err(error) if error.uncertain => intent.state = IntentState::Uncertain,
                Err(_) => return false,
            }

            true
        });
    }

    pub fn confirmed(&self) -> HashSet<u64> {
        self.intents
            .iter()
            .filter(|intent| intent.state == IntentState::Confirmed)
            .map(|intent| intent.id)
            .collect()
    }

    pub fn refreshed(
        &mut self,
        confirmed: &HashSet<u64>,
        repositories: &[RepositoryView],
        loaded_comparisons: &HashSet<PathBuf>,
    ) {
        self.intents.retain(|intent| {
            !confirmed.contains(&intent.id)
                || !loaded_comparisons.contains(intent.snapshot.repository().root())
                || !repositories.iter().any(|repository| {
                    repository.snapshot.repository().root() == intent.snapshot.repository().root()
                        && repository.snapshot.repository().lifetime()
                            == intent.snapshot.repository().lifetime()
                })
        });
    }

    pub fn settle_uncertain(&mut self, root: &Path, applied: bool, refreshed: bool) {
        self.intents.retain_mut(|intent| {
            if intent.snapshot.repository().root() != root || intent.state != IntentState::Uncertain
            {
                return true;
            }

            intent.state = IntentState::Confirmed;
            intent.succeeded.store(applied, Ordering::Release);
            applied && !refreshed
        });
    }

    pub fn forget_root(&mut self, root: &Path) {
        self.intents
            .retain(|intent| intent.snapshot.repository().root() != root);
    }

    pub fn hides(
        &self,
        repository: &RepositoryView,
        revision: &Revision,
        file: &ChangedFile,
    ) -> bool {
        self.intents.iter().any(|intent| {
            intent.snapshot.repository().root() == repository.snapshot.repository().root()
                && intent.snapshot.repository().lifetime()
                    == repository.snapshot.repository().lifetime()
                && match intent.source_category {
                    None => {
                        matches!(intent.source.scope, DiffScope::Change(_))
                            && intent.source.identity == revision.revset()
                    }
                    Some(ClassificationTarget::Comparison) => false,
                    Some(ClassificationTarget::Category(category)) => {
                        if intent
                            .source
                            .revision
                            .as_ref()
                            .and_then(Category::from_revision)
                            == Some(category)
                        {
                            intent.source.identity == revision.revset()
                        } else {
                            repository
                                .comparison
                                .as_ref()
                                .and_then(|comparison| {
                                    comparison
                                        .categories
                                        .iter()
                                        .find(|entry| entry.category == category)
                                })
                                .is_some_and(|entry| {
                                    entry.revision.full_change_id == revision.full_change_id
                                })
                        }
                    }
                }
                && intent
                    .files
                    .iter()
                    .any(|selection| selection.ranges.is_none() && selection.file.path == file.path)
        })
    }

    #[cfg(test)]
    pub fn files(&self, repository: &RepositoryView) -> Vec<FileRow> {
        self.files_with_mutations(repository, &MutationPreviews::default())
    }

    pub fn files_with_mutations(
        &self,
        repository: &RepositoryView,
        mutations: &MutationPreviews,
    ) -> Vec<FileRow> {
        let Some(comparison) = &repository.comparison else {
            return Vec::new();
        };
        let mut rows = comparison
            .files
            .iter()
            .map(|file| {
                FileRow::new(
                    repository,
                    comparison.diff_target(),
                    file.clone(),
                    "comparison",
                    true,
                )
            })
            .chain(comparison.categories.iter().flat_map(|entry| {
                entry.files.iter().map(|file| {
                    FileRow::new(
                        repository,
                        super::super::kk_diff::DiffTarget::change(&entry.revision),
                        file.clone(),
                        "category",
                        false,
                    )
                })
            }))
            .collect::<Vec<_>>();
        mutations.project(repository, None, &mut rows);
        for intent in self.intents.iter().filter(|intent| {
            intent.snapshot.repository().root() == repository.snapshot.repository().root()
                && intent.snapshot.repository().lifetime()
                    == repository.snapshot.repository().lifetime()
                && intent.comparison == comparison.intent.target_revset
        }) {
            for selection in &intent.files {
                let source = rows.iter().position(|row| {
                    row.file.path == selection.file.path
                        && match intent.source_category {
                            Some(ClassificationTarget::Category(category)) => {
                                row.category == Some(category)
                                    && (intent
                                        .source
                                        .revision
                                        .as_ref()
                                        .and_then(Category::from_revision)
                                        != Some(category)
                                        || row.target.identity == intent.source.identity)
                            }
                            Some(ClassificationTarget::Comparison) => row.category.is_none(),
                            None => row.target.identity == intent.source.identity,
                        }
                });
                let mut destination =
                    source.map(|index| rows[index].clone()).unwrap_or_else(|| {
                        FileRow::new(
                            repository,
                            intent.source.clone(),
                            selection.file.clone(),
                            "category",
                            false,
                        )
                    });
                if let Some(index) = source {
                    if selection.ranges.is_none() {
                        rows.remove(index);
                    } else if let Some(row) = rows.get_mut(index) {
                        row.pending = true;
                        row.classification_dependency = Some(intent.succeeded.clone());
                        row.capture_saved = intent.capture_saved;
                    }
                }

                let category = match intent.destination {
                    ClassificationTarget::Category(category) => Some(category),
                    ClassificationTarget::Comparison => None,
                };
                if let Some(existing) = rows
                    .iter_mut()
                    .find(|row| row.category == category && row.file.path == selection.file.path)
                {
                    existing.pending = true;
                    existing.pending_action = None;
                    existing.classification_dependency = Some(intent.succeeded.clone());
                    existing.capture_saved = intent.capture_saved;
                } else {
                    destination.category = category;
                    destination.target.comparison = Some(comparison.intent.clone());
                    destination.pending = true;
                    destination.pending_action = None;
                    destination.classification_dependency = Some(intent.succeeded.clone());
                    destination.capture_saved = intent.capture_saved;
                    destination.id.section = match category {
                        Some(category) => format!("category:{}", category.label()),
                        None => format!("comparison:{}", comparison.diff_target().identity),
                    };
                    rows.push(destination);
                }
            }
        }

        rows
    }
}
