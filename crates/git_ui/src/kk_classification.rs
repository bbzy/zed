use super::{kk_comparison::ComparisonIntent, kk_diff::DiffTarget};
use futures::channel::oneshot;
use gpui::{App, BackgroundExecutor, Entity};
use kk_workbench::{
    ChangedFile, ClassificationPreview, ClassificationRequest, ClassificationTarget, DiffScope,
    FileConflict, FileMoveDirection, FileSelection, PreparedConflictResolution,
    PreparedFileMutation, Publication, SavedSnapshot, Snapshot,
};
use std::{
    cell::RefCell,
    collections::{HashMap, VecDeque},
    rc::{Rc, Weak},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Clone)]
pub(super) struct ClassificationFailure {
    pub message: String,
    pub uncertain: bool,
}

#[derive(Clone)]
pub(super) struct QueuedPreview {
    pub model: Entity<ClassificationPreview>,
    pub id: u64,
    pub file: ChangedFile,
}

pub(super) struct ClassificationJob {
    pub snapshot: Snapshot,
    pub target: DiffTarget,
    pub intent: ComparisonIntent,
    pub files: Vec<FileSelection>,
    pub category: ClassificationTarget,
    pub completions: Vec<oneshot::Sender<Result<Snapshot, ClassificationFailure>>>,
    pub previews: Vec<QueuedPreview>,
    pub list_previews: Vec<u64>,
    pub source_category: Option<ClassificationTarget>,
    pub source_dependency: Option<WriteDependency>,
    pub saved: Vec<SavedSnapshot>,
    fixed_files: Vec<FileSelection>,
    generation: u64,
}

pub(super) struct ClassificationCompletion {
    pub publication: Option<Publication>,
    pub snapshot: Snapshot,
    pub published_snapshot: Option<Snapshot>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum FileMutationAction {
    Revert,
    Move(FileMoveDirection),
}

impl FileMutationAction {
    pub fn label(self) -> &'static str {
        match self {
            Self::Revert => "Revert File",
            Self::Move(FileMoveDirection::Parent) => "Move File Changes to Parent",
            Self::Move(FileMoveDirection::Child) => "Move File Changes to Child",
        }
    }
}

#[derive(Clone)]
pub(super) struct FileMutationRequest {
    pub snapshot: Snapshot,
    pub target: DiffTarget,
    pub file: ChangedFile,
    pub action: FileMutationAction,
}

impl FileMutationRequest {
    pub async fn advance(&mut self, snapshot: Snapshot) -> anyhow::Result<()> {
        self.target = if matches!(self.target.scope, DiffScope::Comparison { .. })
            && let Some(intent) = &self.target.comparison
        {
            super::kk_comparison::Comparison::load(&snapshot, intent.clone())
                .await?
                .diff_target()
        } else {
            self.target.resolve(&snapshot).await?.0
        };
        self.snapshot = snapshot;
        Ok(())
    }

    pub async fn prepare(
        self,
        advance_comparison: bool,
    ) -> anyhow::Result<(Snapshot, Option<PreparedFileMutation>)> {
        let current = self.snapshot.repository().snapshot_before_write().await?;
        let (scope, prepared_source) = match self.action {
            FileMutationAction::Move(direction) => {
                let DiffScope::Change(revision) = &self.target.scope else {
                    anyhow::bail!("Select a file in a single change before moving it");
                };
                let source = self.snapshot.exact_revision(revision).await?;
                let current_source = current.exact_revision(&source.revset()).await?;
                let relation = match direction {
                    FileMoveDirection::Parent => "parents",
                    FileMoveDirection::Child => "children",
                };

                let destination = self
                    .snapshot
                    .exact_revision(&format!("{relation}({})", source.full_commit_id))
                    .await?;
                let current_destination = current
                    .exact_revision(&format!("{relation}({})", current_source.full_commit_id))
                    .await?;
                anyhow::ensure!(
                    destination.full_change_id == current_destination.full_change_id,
                    "The move destination changed while this action was pending"
                );
                current
                    .verify_file_version(
                        &source.full_commit_id,
                        &current_source.full_commit_id,
                        &self.file,
                    )
                    .await?;
                current
                    .verify_file_version(
                        &destination.full_commit_id,
                        &current_destination.full_commit_id,
                        &self.file,
                    )
                    .await?;
                (
                    DiffScope::Change(current_source.full_commit_id.clone()),
                    Some((current_source, direction)),
                )
            }
            FileMutationAction::Revert => {
                let before = self.snapshot.exact_revision("@").await?;
                let selected = match &self.target.scope {
                    DiffScope::Change(revision) => revision,
                    DiffScope::Comparison { target, .. } => target,
                };

                anyhow::ensure!(
                    self.snapshot.exact_revision(selected).await?.full_commit_id
                        == before.full_commit_id,
                    "Revert is only available for the working copy or a comparison with it"
                );
                let working_copy = current.exact_revision("@").await?;
                anyhow::ensure!(
                    before.full_change_id == working_copy.full_change_id,
                    "The working-copy change was switched while this action was pending"
                );
                current
                    .verify_file_version(
                        &before.full_commit_id,
                        &working_copy.full_commit_id,
                        &self.file,
                    )
                    .await?;
                let scope = match &self.target.scope {
                    DiffScope::Change(_) => DiffScope::Comparison {
                        base: self
                            .snapshot
                            .exact_revision(&format!("parents({})", before.full_commit_id))
                            .await?
                            .full_commit_id,
                        target: "@".into(),
                    },
                    DiffScope::Comparison { base, .. } => {
                        let base =
                            if advance_comparison && let Some(intent) = &self.target.comparison {
                                current
                                    .comparison_boundary(
                                        &intent.target_revset,
                                        intent.range_start_change_id.as_deref(),
                                    )
                                    .await?
                                    .base_revset
                            } else {
                                self.snapshot.exact_revision(base).await?.full_commit_id
                            };

                        DiffScope::Comparison {
                            base,
                            target: "@".into(),
                        }
                    }
                };

                (scope, None)
            }
        };

        let file = current
            .changed_files(&scope)
            .await?
            .into_iter()
            .find(|file| file.path == self.file.path);
        let Some(file) = file else {
            return Ok((current, None));
        };
        anyhow::ensure!(
            file.renamed_from == self.file.renamed_from,
            "The selected file's rename relationship changed while this action was pending"
        );
        let prepared = match prepared_source {
            Some((source, direction)) => current
                .prepare_file_move(&source, direction, &file)
                .await
                .map(Some),
            None => current.prepare_file_revert(&scope, &file).await.map(Some),
        }?;
        Ok((current, prepared))
    }
}

pub(super) struct FileMutationJob {
    pub request: FileMutationRequest,
    pub preview_id: Option<u64>,
    completions: Rc<RefCell<Vec<oneshot::Sender<Result<Snapshot, ClassificationFailure>>>>>,
    pub dependencies: Vec<WriteDependency>,
}

impl FileMutationJob {
    pub fn new(
        request: FileMutationRequest,
        completion: oneshot::Sender<Result<Snapshot, ClassificationFailure>>,
        dependencies: Vec<WriteDependency>,
    ) -> Self {
        Self {
            request,
            preview_id: None,
            completions: Rc::new(RefCell::new(vec![completion])),
            dependencies,
        }
    }

    pub fn take_completions(
        &self,
    ) -> Vec<oneshot::Sender<Result<Snapshot, ClassificationFailure>>> {
        std::mem::take(&mut *self.completions.borrow_mut())
    }

    fn key(&self) -> FileMutationKey {
        FileMutationKey {
            action: self.request.action,
            source: self.request.target.identity.clone(),
            operation: self.request.snapshot.operation().into(),
            lifetime: self.request.snapshot.repository().lifetime().cloned(),
            path: self.request.file.path.clone(),
        }
    }
}

#[derive(PartialEq, Eq, Hash)]
struct FileMutationKey {
    action: FileMutationAction,
    source: String,
    operation: String,
    lifetime: Option<kk_workbench::RepositoryLifetime>,
    path: String,
}

type FileWaiters = Weak<RefCell<Vec<oneshot::Sender<Result<Snapshot, ClassificationFailure>>>>>;

pub(super) type WriteDependency = Arc<AtomicBool>;

pub(super) enum RepositoryJob {
    Classification(ClassificationJob),
    File(FileMutationJob),
    Conflict(ConflictJob),
    Sparse(SparseCheckoutJob),
    UpdateStale(UpdateStaleJob),
}

pub(super) struct UpdateStaleJob {
    pub snapshot: Snapshot,
    pub completion: oneshot::Sender<Result<Snapshot, ClassificationFailure>>,
}

pub(super) struct SparseCheckoutJob {
    pub snapshot: Snapshot,
    pub paths: Vec<String>,
    pub current: Arc<dyn Fn() -> bool + Send + Sync>,
    pub completion: oneshot::Sender<Result<Snapshot, ClassificationFailure>>,
}

#[derive(Clone)]
pub(super) struct ConflictRequest {
    pub snapshot: Snapshot,
    pub conflict: FileConflict,
    pub result: Option<Vec<u8>>,
    pub executable: Option<bool>,
}

impl ConflictRequest {
    pub async fn prepare(
        self,
        acknowledged: Snapshot,
    ) -> anyhow::Result<PreparedConflictResolution> {
        let commit = acknowledged
            .exact_revision(&self.conflict.revset())
            .await?
            .full_commit_id;
        self.snapshot
            .repository()
            .snapshot_before_write()
            .await?
            .prepare_conflict_resolution(
                &self.conflict,
                &commit,
                self.result.as_deref(),
                self.executable,
            )
            .await
    }
}

pub(super) struct ConflictJob {
    pub request: ConflictRequest,
    pub preview_id: Option<u64>,
    pub completion: oneshot::Sender<Result<Snapshot, ClassificationFailure>>,
}

impl RepositoryJob {
    fn snapshot(&self) -> &Snapshot {
        match self {
            Self::Classification(job) => &job.snapshot,
            Self::File(job) => &job.request.snapshot,
            Self::Conflict(job) => &job.request.snapshot,
            Self::Sparse(job) => &job.snapshot,
            Self::UpdateStale(job) => &job.snapshot,
        }
    }

    fn paths(&self) -> Vec<String> {
        let files = match self {
            Self::Classification(job) => job
                .files
                .iter()
                .map(|selection| &selection.file)
                .collect::<Vec<_>>(),
            Self::File(job) => vec![&job.request.file],
            Self::Conflict(job) => return vec![job.request.conflict.path.clone()],
            // Materialization does not rewrite history and may yield to saved-file captures.
            // Later writes must not depend on a checkout that can be cancelled or deferred.
            Self::Sparse(_) => return Vec::new(),
            Self::UpdateStale(_) => return Vec::new(),
        };

        files
            .into_iter()
            .flat_map(|file| std::iter::once(file.path.clone()).chain(file.renamed_from.clone()))
            .collect()
    }
}

pub(super) struct QueuedWrite {
    pub job: RepositoryJob,
    pub waiting: Option<kk_workbench::PerformanceSpan>,
    pub successes: Vec<Arc<AtomicBool>>,
    dependencies: Vec<Arc<AtomicBool>>,
}

impl QueuedWrite {
    pub fn has_dependencies(&self) -> bool {
        !self.dependencies.is_empty()
    }

    pub fn check_dependencies(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.dependencies
                .iter()
                .all(|success| success.load(Ordering::Acquire)),
            "An earlier operation on this file failed. Your queued action was not applied."
        );
        Ok(())
    }
}

impl ClassificationJob {
    pub fn new(
        snapshot: Snapshot,
        target: DiffTarget,
        intent: ComparisonIntent,
        files: Vec<FileSelection>,
        category: ClassificationTarget,
        completion: oneshot::Sender<Result<Snapshot, ClassificationFailure>>,
        previews: Vec<QueuedPreview>,
    ) -> anyhow::Result<Self> {
        let saved = SavedSnapshot::capture(
            snapshot.repository(),
            files.iter().flat_map(|selection| {
                std::iter::once(selection.file.path.clone())
                    .chain(selection.file.renamed_from.clone())
            }),
        )?;
        let mut job = Self::without_saved(
            snapshot, target, intent, files, category, completion, previews,
        );
        job.saved.push(saved);
        Ok(job)
    }

    pub fn without_saved(
        snapshot: Snapshot,
        target: DiffTarget,
        intent: ComparisonIntent,
        files: Vec<FileSelection>,
        category: ClassificationTarget,
        completion: oneshot::Sender<Result<Snapshot, ClassificationFailure>>,
        previews: Vec<QueuedPreview>,
    ) -> Self {
        let fixed_files = files
            .iter()
            .filter(|file| {
                !previews
                    .iter()
                    .any(|preview| preview.file.path == file.file.path)
            })
            .cloned()
            .collect();
        Self {
            snapshot,
            target,
            intent,
            files,
            category,
            completions: vec![completion],
            previews,
            list_previews: Vec::new(),
            source_category: None,
            source_dependency: None,
            saved: Vec::new(),
            fixed_files,
            generation: 0,
        }
    }

    fn compatible(&self, other: &Self) -> bool {
        self.source_category.is_none()
            && other.source_category.is_none()
            && self.source_dependency.is_none()
            && other.source_dependency.is_none()
            && self.generation == other.generation
            && self
                .saved
                .first()
                .and_then(SavedSnapshot::version)
                .is_some_and(|version| {
                    other.saved.first().and_then(SavedSnapshot::version) == Some(version)
                })
            && self.snapshot.operation() == other.snapshot.operation()
            && self.snapshot.repository().lifetime() == other.snapshot.repository().lifetime()
            && self.target.scope == other.target.scope
            && self.category == other.category
            && self.intent.target_revset == other.intent.target_revset
            && self.intent.range_start_change_id == other.intent.range_start_change_id
            && self.files.iter().all(|file| {
                other
                    .files
                    .iter()
                    .all(|other| file.file.path != other.file.path || file.file == other.file)
            })
    }

    fn merge(&mut self, other: Self) {
        self.completions.extend(other.completions);
        self.previews.extend(other.previews);
        self.list_previews.extend(other.list_previews);
        self.saved.extend(other.saved);
        self.fixed_files.extend(other.fixed_files);
        Self::merge_files(&mut self.files, other.files);
    }

    fn merge_files(files: &mut Vec<FileSelection>, selections: Vec<FileSelection>) {
        for selection in selections {
            if let Some(existing) = files
                .iter_mut()
                .find(|file| file.file.path == selection.file.path)
            {
                match (&mut existing.ranges, selection.ranges) {
                    (Some(existing), Some(ranges)) => existing.extend(ranges),
                    (existing, None) => *existing = None,
                    (None, _) => {}
                }
            } else {
                files.push(selection);
            }
        }
    }

    pub fn current_files(&self, cx: &App) -> anyhow::Result<Vec<FileSelection>> {
        let mut files = Vec::new();
        Self::merge_files(&mut files, self.fixed_files.clone());
        for preview in &self.previews {
            let model = preview.model.read(cx);
            let whole = model
                .selection(preview.id)?
                .is_some_and(|selection| selection.whole_file);
            let ranges = if whole {
                None
            } else {
                Some(model.native_ranges(preview.id)?)
            };

            if ranges.as_ref().is_none_or(|ranges| !ranges.is_empty()) {
                Self::merge_files(
                    &mut files,
                    vec![FileSelection {
                        file: preview.file.clone(),
                        ranges,
                    }],
                );
            }
        }

        Ok(files)
    }

    pub async fn execute_queued(
        self,
        snapshot: Snapshot,
        files: Vec<FileSelection>,
    ) -> anyhow::Result<ClassificationCompletion> {
        if self.source_category.is_none() && self.source_dependency.is_none() {
            return Self::execute(
                snapshot,
                self.snapshot,
                self.target,
                self.intent,
                files,
                self.category,
                self.saved,
            )
            .await;
        }
        let result = async {
            let comparison =
                super::kk_comparison::Comparison::load(&snapshot, self.intent.clone()).await?;
            let (target, available) = match self.source_category {
                None => {
                    let target = if matches!(self.target.scope, DiffScope::Comparison { .. }) {
                        comparison.diff_target()
                    } else {
                        self.target.resolve(&snapshot).await?.0
                    };
                    let files = snapshot.changed_files(&target.scope).await?;
                    (target, files)
                }
                Some(ClassificationTarget::Comparison) => {
                    (comparison.diff_target(), comparison.files)
                }
                Some(ClassificationTarget::Category(category)) => {
                    let entry = if self
                        .target
                        .revision
                        .as_ref()
                        .and_then(kk_workbench::Category::from_revision)
                        == Some(category)
                    {
                        comparison
                            .categories
                            .iter()
                            .find(|entry| entry.revision.revset() == self.target.identity)
                    } else {
                        comparison
                            .categories
                            .iter()
                            .find(|entry| entry.category == category)
                    }
                    .ok_or_else(|| {
                        anyhow::anyhow!("The queued classification source no longer exists")
                    })?;
                    (DiffTarget::change(&entry.revision), entry.files.clone())
                }
            };
            let files = files
                .into_iter()
                .map(|selection| {
                    let file = available
                        .iter()
                        .find(|file| file.path == selection.file.path)
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "The queued classification source no longer contains {}",
                                selection.file.path
                            )
                        })?;
                    Ok(FileSelection {
                        file: file.clone(),
                        ranges: selection.ranges,
                    })
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            Self::prepare(
                &snapshot,
                snapshot.clone(),
                target,
                self.intent,
                files,
                self.category,
                self.saved.clone(),
            )
            .await
        }
        .await;
        Self::finish(snapshot, self.saved, result).await
    }

    pub async fn execute(
        snapshot: Snapshot,
        previous: Snapshot,
        target: DiffTarget,
        intent: ComparisonIntent,
        files: Vec<FileSelection>,
        category: ClassificationTarget,
        saved: Vec<SavedSnapshot>,
    ) -> anyhow::Result<ClassificationCompletion> {
        let result = Self::prepare(
            &snapshot,
            previous,
            target,
            intent,
            files,
            category,
            saved.clone(),
        )
        .await;
        Self::finish(snapshot, saved, result).await
    }

    pub async fn finish(
        snapshot: Snapshot,
        saved: Vec<SavedSnapshot>,
        mut result: anyhow::Result<Option<Publication>>,
    ) -> anyhow::Result<ClassificationCompletion> {
        let mut latest = None;
        for captured in saved {
            match captured.finish().await {
                Ok(Some(snapshot)) => latest = Some(snapshot),
                Ok(None) => {}
                Err(error) => {
                    result = match result {
                        Ok(Some(mut publication)) => {
                            let message = format!("Could not record later saved files: {error:#}");
                            publication.synchronization_error =
                                Some(match publication.synchronization_error {
                                    Some(previous) => format!("{previous}; {message}"),
                                    None => message,
                                });
                            Ok(Some(publication))
                        }
                        Ok(None) => Err(error.context("Could not record later saved files")),
                        Err(failure) => Err(failure
                            .context(format!("Could not record later saved files: {error:#}"))),
                    };
                }
            }
        }

        result.map(|mut publication| {
            let published_snapshot = publication
                .as_ref()
                .map(|publication| publication.snapshot.clone());
            let snapshot = latest
                .or_else(|| {
                    publication
                        .as_ref()
                        .map(|publication| publication.snapshot.clone())
                })
                .unwrap_or(snapshot);
            if let Some(publication) = &mut publication {
                publication.snapshot = snapshot.clone();
            }

            ClassificationCompletion {
                publication,
                snapshot,
                published_snapshot,
            }
        })
    }

    async fn prepare(
        snapshot: &Snapshot,
        previous: Snapshot,
        target: DiffTarget,
        intent: ComparisonIntent,
        files: Vec<FileSelection>,
        category: ClassificationTarget,
        saved: Vec<SavedSnapshot>,
    ) -> anyhow::Result<Option<Publication>> {
        if files.is_empty() {
            return Ok(None);
        }

        let source = match &target.scope {
            DiffScope::Change(revision) => Some(
                previous
                    .revisions(Some(&format!("exactly(({revision}), 1)")), 2)
                    .await?
                    .pop()
                    .ok_or_else(|| anyhow::anyhow!("The source change no longer exists"))?,
            ),
            DiffScope::Comparison { .. } => None,
        };

        let request = ClassificationRequest {
            comparison: intent.target_revset,
            range_start: intent.range_start_change_id,
            source,
            target: category,
            files,
        };
        let Some(prepared) = snapshot
            .prepare_classification_with_saved(&previous, &request, saved)
            .await?
        else {
            return Ok(None);
        };
        Ok(Some(prepared.publish().await?.publication))
    }
}

#[derive(Default)]
pub(super) struct RepositoryQueue {
    jobs: VecDeque<QueuedWrite>,
    pending: Vec<(
        Vec<String>,
        Option<kk_workbench::RepositoryLifetime>,
        Arc<AtomicBool>,
    )>,
    pending_files: HashMap<FileMutationKey, FileWaiters>,
    advancements: HashMap<String, Snapshot>,
    running: bool,
    generation: u64,
    executor: Option<BackgroundExecutor>,
    suspended: Option<SuspendedWrite>,
}

struct SuspendedWrite {
    successes: Vec<WriteDependency>,
    snapshots: Vec<Snapshot>,
    previews: Vec<QueuedPreview>,
    completions: Vec<oneshot::Sender<Result<Snapshot, ClassificationFailure>>>,
}

pub(super) struct ResumedQueue {
    pub run: bool,
    pub rejected_previews: Vec<QueuedPreview>,
}

impl Drop for RepositoryQueue {
    fn drop(&mut self) {
        let mut saved = Vec::new();
        for queued in self.jobs.drain(..) {
            let completions = match queued.job {
                RepositoryJob::Classification(job) => {
                    saved.extend(job.saved);
                    job.completions
                }
                RepositoryJob::File(job) => job.take_completions(),
                RepositoryJob::Conflict(job) => vec![job.completion],
                RepositoryJob::Sparse(job) => vec![job.completion],
                RepositoryJob::UpdateStale(job) => vec![job.completion],
            };

            for completion in completions {
                if completion
                    .send(Err(ClassificationFailure {
                        message: "KK Workbench was closed before this operation started".into(),
                        uncertain: false,
                    }))
                    .is_err()
                {
                    log::debug!("KK classification waiter was closed");
                }
            }
        }

        if !saved.is_empty()
            && let Some(executor) = &self.executor
        {
            executor
                .spawn(async move {
                    for captured in saved {
                        if let Err(error) = captured.finish().await {
                            log::error!(
                                "Could not record saved files after closing KK Workbench: {error:#}"
                            );
                        }
                    }
                })
                .detach();
        }
    }
}

impl RepositoryQueue {
    pub fn enqueue_update_stale(&mut self, job: UpdateStaleJob) -> bool {
        self.flush();
        self.enqueue_write(RepositoryJob::UpdateStale(job), Vec::new())
    }

    pub fn defer_sparse(&mut self, queued: QueuedWrite) {
        self.jobs.push_back(queued);
    }

    pub fn enqueue_sparse(&mut self, job: SparseCheckoutJob) -> bool {
        self.enqueue_write(RepositoryJob::Sparse(job), Vec::new())
    }

    pub fn new(executor: BackgroundExecutor) -> Self {
        Self {
            jobs: VecDeque::new(),
            pending: Vec::new(),
            pending_files: HashMap::new(),
            advancements: HashMap::new(),
            running: false,
            generation: 0,
            executor: Some(executor),
            suspended: None,
        }
    }

    pub fn enqueue(&mut self, mut job: ClassificationJob) -> bool {
        job.generation = self.generation;
        let mut dependencies = if job.source_category.is_some() || job.source_dependency.is_some() {
            let paths = job
                .files
                .iter()
                .flat_map(|selection| {
                    std::iter::once(selection.file.path.clone())
                        .chain(selection.file.renamed_from.clone())
                })
                .collect::<Vec<_>>();
            self.dependencies(&paths, job.snapshot.repository().lifetime())
        } else {
            Vec::new()
        };
        if let Some(dependency) = &job.source_dependency {
            dependencies.push(dependency.clone());
        }
        self.enqueue_write(RepositoryJob::Classification(job), dependencies)
    }

    pub fn enqueue_file(&mut self, mut job: FileMutationJob) -> bool {
        self.pending_files
            .retain(|_, waiters| waiters.strong_count() > 0);
        let key = job.key();
        if let Some(waiters) = self.pending_files.get(&key).and_then(Weak::upgrade) {
            waiters.borrow_mut().extend(job.take_completions());
            return false;
        }

        self.pending_files
            .insert(key, Rc::downgrade(&job.completions));
        self.flush();
        let dependencies = std::mem::take(&mut job.dependencies);
        self.enqueue_write(RepositoryJob::File(job), dependencies)
    }

    pub fn enqueue_conflict(&mut self, job: ConflictJob) -> bool {
        let dependencies = self.dependencies(
            std::slice::from_ref(&job.request.conflict.path),
            job.request.snapshot.repository().lifetime(),
        );
        self.flush();
        self.enqueue_write(RepositoryJob::Conflict(job), dependencies)
    }

    pub fn dependencies(
        &self,
        paths: &[String],
        lifetime: Option<&kk_workbench::RepositoryLifetime>,
    ) -> Vec<Arc<AtomicBool>> {
        self.pending
            .iter()
            .filter(|(resources, pending_lifetime, _)| {
                pending_lifetime.as_ref() == lifetime
                    && resources.iter().any(|path| paths.contains(path))
            })
            .map(|(_, _, success)| success.clone())
            .collect()
    }

    fn enqueue_write(&mut self, job: RepositoryJob, dependencies: Vec<Arc<AtomicBool>>) -> bool {
        let paths = job.paths();
        let waiting = job.snapshot().repository().diagnostics().start("queue.wait", None,
            serde_json::json!({"root": job.snapshot().repository().root(), "files": paths, "depth": self.jobs.len()}));
        let success = Arc::new(AtomicBool::new(false));
        self.pending.push((
            paths,
            job.snapshot().repository().lifetime().cloned(),
            success.clone(),
        ));
        self.jobs.push_back(QueuedWrite {
            job,
            waiting: Some(waiting),
            successes: vec![success],
            dependencies,
        });
        !std::mem::replace(&mut self.running, true)
    }

    pub fn flush(&mut self) {
        self.generation += 1;
    }

    pub fn next(&mut self) -> Option<(QueuedWrite, Snapshot)> {
        if self.suspended.is_some() {
            return None;
        }

        let Some(mut batch) = self.jobs.pop_front() else {
            self.running = false;
            return None;
        };
        while self.jobs.front().is_some_and(|next| matches!((&batch.job, &next.job), (RepositoryJob::Classification(batch), RepositoryJob::Classification(next)) if batch.compatible(next))) {
            if let Some(QueuedWrite { job: RepositoryJob::Classification(next), successes, waiting, .. }) = self.jobs.pop_front()
                && let RepositoryJob::Classification(job) = &mut batch.job {
                if let Some(waiting) = waiting { waiting.finish("coalesced", serde_json::Value::Null); }
                job.merge(next);
                batch.successes.extend(successes);
            }
        }

        let snapshot = self
            .advancements
            .get(batch.job.snapshot().operation())
            .cloned()
            .unwrap_or_else(|| batch.job.snapshot().clone())
            .with_lifetime(batch.job.snapshot().repository().lifetime().cloned());
        Some((batch, snapshot))
    }

    pub fn complete(&mut self, successes: &[Arc<AtomicBool>], succeeded: bool) {
        for success in successes {
            success.store(succeeded, Ordering::Release);
        }

        self.pending.retain(|(_, _, pending)| {
            !successes
                .iter()
                .any(|success| Arc::ptr_eq(success, pending))
        });
    }

    pub fn suspend(
        &mut self,
        successes: Vec<WriteDependency>,
        snapshots: Vec<Snapshot>,
        previews: Vec<QueuedPreview>,
    ) {
        self.suspended = Some(SuspendedWrite {
            successes,
            snapshots,
            previews,
            completions: Vec::new(),
        });
    }

    pub fn retain_conflict_waiters(
        &mut self,
        completions: &mut Vec<oneshot::Sender<Result<Snapshot, ClassificationFailure>>>,
    ) {
        if let Some(suspended) = &mut self.suspended {
            suspended.completions.append(completions);
        }
    }

    pub fn resume(&mut self, applied: bool, current: &Snapshot) -> Option<ResumedQueue> {
        let suspended = self.suspended.take()?;
        self.complete(&suspended.successes, applied);
        if applied {
            for snapshot in suspended.snapshots {
                self.advance(&snapshot, &snapshot, current);
            }
        }

        self.running = !self.jobs.is_empty();
        for completion in suspended.completions {
            let outcome = if applied {
                Ok(current.clone())
            } else {
                Err(ClassificationFailure {
                    message: "The conflict resolution was not applied. Your draft is retained."
                        .into(),
                    uncertain: false,
                })
            };
            if completion.send(outcome).is_err() {
                log::debug!("KK conflict waiter was closed");
            }
        }

        Some(ResumedQueue {
            run: self.running,
            rejected_previews: if applied {
                Vec::new()
            } else {
                suspended.previews
            },
        })
    }

    pub fn advance(&mut self, previous: &Snapshot, original: &Snapshot, next: &Snapshot) {
        for advanced in self.advancements.values_mut() {
            if advanced.operation() == previous.operation() {
                *advanced = next.clone();
            }
        }

        self.advancements
            .insert(previous.operation().into(), next.clone());
        self.advancements
            .insert(original.operation().into(), next.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kk_panel::remote_tests::run_kk;
    use anyhow::{Context as _, Result};
    use gpui::AppContext as _;
    use kk_workbench::{Category, LineRange, Repository, SelectionSide};
    use std::path::Path;

    #[test]
    fn interleaved_writes_preserve_order_and_cancel_only_failed_dependencies() -> Result<()> {
        futures::executor::block_on(async {
            for reject_first in [false, true] {
                let directory = tempfile::tempdir()?;
                for arguments in [vec!["init", "."], vec!["new", "-m", "source"]] {
                    run_kk(directory.path(), &arguments).await?;
                }

                std::fs::write(directory.path().join("first.txt"), "first\nremaining\n")?;
                std::fs::write(directory.path().join("second.txt"), "second\n")?;
                let repository = Repository::open(directory.path(), Path::new("kk")).await?;
                let snapshot = repository.snapshot(true).await?;
                let base = snapshot.exact_revision("@-").await?;
                let comparison = super::super::kk_comparison::Comparison::load(
                    &snapshot,
                    super::super::kk_comparison::Comparison::new(&base).intent,
                )
                .await?;
                let first_file = comparison
                    .files
                    .iter()
                    .find(|file| file.path == "first.txt")
                    .context("first file")?
                    .clone();
                let second_file = comparison
                    .files
                    .iter()
                    .find(|file| file.path == "second.txt")
                    .context("second file")?
                    .clone();
                let mut queue = RepositoryQueue::default();
                let (sender, first_completion) = oneshot::channel();
                queue.enqueue(ClassificationJob::new(
                    snapshot.clone(),
                    comparison.diff_target(),
                    comparison.intent.clone(),
                    vec![FileSelection {
                        file: first_file.clone(),
                        ranges: Some(vec![LineRange {
                            side: SelectionSide::Modified,
                            begin: if reject_first { 999 } else { 1 },
                            end: if reject_first { 999 } else { 1 },
                        }]),
                    }],
                    ClassificationTarget::Category(Category::Reviewed),
                    sender,
                    Vec::new(),
                )?);
                let dependencies = queue.dependencies(std::slice::from_ref(&first_file.path), None);
                let (sender, revert_completion) = oneshot::channel();
                queue.enqueue_file(FileMutationJob::new(
                    FileMutationRequest {
                        snapshot: snapshot.clone(),
                        target: comparison.diff_target(),
                        file: first_file,
                        action: FileMutationAction::Revert,
                    },
                    sender,
                    dependencies,
                ));
                let (sender, second_completion) = oneshot::channel();
                queue.enqueue(ClassificationJob::new(
                    snapshot.clone(),
                    comparison.diff_target(),
                    comparison.intent.clone(),
                    vec![FileSelection {
                        file: second_file,
                        ranges: None,
                    }],
                    ClassificationTarget::Category(Category::Reviewed),
                    sender,
                    Vec::new(),
                )?);
                let mut order = Vec::new();
                let mut duplicates = Vec::new();
                while let Some((queued, current)) = queue.next() {
                    let dependency = queued.check_dependencies();
                    let advance_comparison = queued.has_dependencies();
                    let original = queued.job.snapshot().clone();
                    let previous = current.clone();
                    let (result, completions) = match queued.job {
                        RepositoryJob::Classification(job) => {
                            dependency?;
                            order.push(job.files[0].file.path.clone());
                            let result = ClassificationJob::execute(
                                current,
                                job.snapshot,
                                job.target,
                                job.intent,
                                job.files,
                                job.category,
                                job.saved,
                            )
                            .await
                            .map(|result| result.snapshot);
                            (result, job.completions)
                        }
                        RepositoryJob::File(job) => {
                            order.push("revert".into());
                            let (sender, completed) = oneshot::channel();
                            duplicates.push(completed);
                            assert!(!queue.enqueue_file(FileMutationJob::new(
                                job.request.clone(),
                                sender,
                                Vec::new()
                            )));
                            let result = async {
                                dependency?;
                                let (_, prepared) =
                                    job.request.clone().prepare(advance_comparison).await?;
                                let publication = prepared
                                    .context("remaining first-file change")?
                                    .publish()
                                    .await?;
                                assert_eq!(
                                    std::fs::read(directory.path().join("first.txt"))?,
                                    b"first\n"
                                );
                                anyhow::Ok(publication.snapshot)
                            }
                            .await;
                            if reject_first {
                                assert!(
                                    result
                                        .as_ref()
                                        .err()
                                        .context("dependent revert must fail")?
                                        .to_string()
                                        .contains("earlier operation")
                                );
                            }

                            (result, job.take_completions())
                        }
                        RepositoryJob::Conflict(_)
                        | RepositoryJob::Sparse(_)
                        | RepositoryJob::UpdateStale(_) => {
                            anyhow::bail!("Unexpected job in file queue test")
                        }
                    };

                    if order.len() == 1 {
                        queue.suspend(
                            queued.successes.clone(),
                            vec![previous.clone(), original.clone()],
                            Vec::new(),
                        );
                        let (completion, mut outcome) = oneshot::channel();
                        let mut retained = vec![completion];
                        queue.retain_conflict_waiters(&mut retained);
                        assert!(retained.is_empty());
                        assert!(outcome.try_recv()?.is_none());
                        assert!(
                            queue.next().is_none(),
                            "an uncertain write must retain the later jobs"
                        );
                        let settled = result.as_ref().unwrap_or(&previous);
                        assert!(
                            queue
                                .resume(result.is_ok(), settled)
                                .context("resume suspended queue")?
                                .run
                        );
                        assert_eq!(outcome.await?.is_err(), result.is_err());
                    } else {
                        queue.complete(&queued.successes, result.is_ok());
                    }

                    if let Ok(next) = &result {
                        queue.advance(&previous, &original, next);
                    }

                    let result = result.map_err(|error| ClassificationFailure {
                        message: format!("{error:#}"),
                        uncertain: false,
                    });
                    for completion in completions {
                        assert!(completion.send(result.clone()).is_ok());
                    }
                }

                assert_eq!(order, ["first.txt", "revert", "second.txt"]);
                assert_eq!(first_completion.await?.is_err(), reject_first);
                assert_eq!(revert_completion.await?.is_err(), reject_first);
                assert!(second_completion.await?.is_ok());
                for duplicate in duplicates {
                    assert_eq!(duplicate.await?.is_err(), reject_first);
                }

                assert_eq!(
                    std::fs::read(directory.path().join("first.txt"))?,
                    if reject_first {
                        b"first\nremaining\n".as_slice()
                    } else {
                        b"first\n"
                    }
                );
                let finished = repository.snapshot(false).await?;
                let comparison =
                    super::super::kk_comparison::Comparison::load(&finished, comparison.intent)
                        .await?;
                assert_eq!(comparison.files.len(), usize::from(reject_first));
                assert_eq!(comparison.categories.len(), 1);
                assert!(
                    comparison.categories[0]
                        .files
                        .iter()
                        .any(|file| file.path == "second.txt")
                );
                let category = &comparison.categories[0];
                let file = category
                    .files
                    .iter()
                    .find(|file| file.path == "second.txt")
                    .context("classified second file")?
                    .clone();
                let request = FileMutationRequest {
                    snapshot: finished,
                    target: DiffTarget::change(&category.revision),
                    file,
                    action: FileMutationAction::Move(FileMoveDirection::Child),
                };
                std::fs::write(directory.path().join("unrelated.txt"), "unrelated save\n")?;
                let (_, prepared) = request.prepare(false).await?;
                let moved = prepared.context("queued move")?.publish().await?.snapshot;
                assert_eq!(
                    moved.file_content("@", "unrelated.txt").await?,
                    b"unrelated save\n"
                );
                let file = moved
                    .changed_files(&DiffScope::Change("@".into()))
                    .await?
                    .into_iter()
                    .find(|file| file.path == "second.txt")
                    .context("moved second file")?;
                let working_copy = moved.exact_revision("@").await?;
                let request = FileMutationRequest {
                    snapshot: moved,
                    target: DiffTarget::change(&working_copy),
                    file,
                    action: FileMutationAction::Revert,
                };
                std::fs::write(directory.path().join("second.txt"), "later selected save\n")?;
                let error = request
                    .prepare(false)
                    .await
                    .err()
                    .context("later save must reject stale revert")?;
                assert!(error.to_string().contains("selected file changed"));
                assert_eq!(
                    std::fs::read(directory.path().join("second.txt"))?,
                    b"later selected save\n"
                );
                assert_eq!(
                    std::fs::read(directory.path().join("unrelated.txt"))?,
                    b"unrelated save\n"
                );
            }

            Ok(())
        })
    }

    #[gpui::test(iterations = 3)]
    async fn closing_a_queue_records_later_saves_without_classifying(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.background_executor.allow_parking();
        let (directory, snapshot, comparison) = cx
            .background_spawn(async {
                let directory = tempfile::tempdir()?;
                for arguments in [vec!["init", "."], vec!["new", "-m", "source"]] {
                    run_kk(directory.path(), &arguments).await?;
                }

                std::fs::write(directory.path().join("file.txt"), "saved\n")?;
                let repository = Repository::open(directory.path(), Path::new("kk")).await?;
                let snapshot = repository.snapshot(true).await?;
                let base = snapshot
                    .revisions(Some("@-"), 1)
                    .await?
                    .pop()
                    .context("base")?;
                let comparison = super::super::kk_comparison::Comparison::load(
                    &snapshot,
                    super::super::kk_comparison::Comparison::new(&base).intent,
                )
                .await?;
                anyhow::Ok((directory, snapshot, comparison))
            })
            .await
            .expect("prepare isolated repository");
        let mut queue = RepositoryQueue::new(cx.background_executor.clone());
        let (sender, completed) = oneshot::channel();
        let file = comparison.files.first().expect("changed file").clone();
        queue.enqueue(
            ClassificationJob::new(
                snapshot.clone(),
                comparison.diff_target(),
                comparison.intent,
                vec![FileSelection { file, ranges: None }],
                ClassificationTarget::Category(Category::Reviewed),
                sender,
                Vec::new(),
            )
            .expect("capture queued input"),
        );
        std::fs::write(directory.path().join("file.txt"), "later saved\n")
            .expect("save later version");
        kk_workbench::record_saved_file(snapshot.repository().root(), "file.txt")
            .expect("record native save");
        drop(queue);
        let cancelled = completed
            .await
            .expect("cancelled waiter")
            .err()
            .expect("must cancel");
        assert!(!cancelled.uncertain);
        let mut recorded = false;
        for _ in 0..100 {
            let repository = snapshot.repository().clone();
            let (contents, categories) = cx
                .background_spawn(async move {
                    let snapshot = repository.snapshot(false).await?;
                    let contents = snapshot.file_content("@", "file.txt").await?;
                    let categories = snapshot
                        .revisions(Some("description(exact:\"KKW: REVIEWED\")"), 2)
                        .await?;
                    anyhow::Ok((contents, categories))
                })
                .await
                .expect("read closed queue result");
            assert!(categories.is_empty());
            if contents == b"later saved\n" {
                recorded = true;
                break;
            }

            cx.background_executor
                .timer(std::time::Duration::from_millis(1))
                .await;
        }

        assert!(recorded, "closing the queue must record later native saves");
        assert_eq!(
            std::fs::read(directory.path().join("file.txt")).expect("native file"),
            b"later saved\n"
        );
    }

    #[test]
    fn queued_category_changes_resolve_new_sources_after_their_prerequisites() -> Result<()> {
        futures::executor::block_on(async {
            let directory = tempfile::tempdir()?;
            run_kk(directory.path(), &["init", "."]).await?;
            std::fs::write(directory.path().join("file.txt"), "base\n")?;
            run_kk(directory.path(), &["new", "-m", "work"]).await?;
            std::fs::write(directory.path().join("file.txt"), "changed\n")?;
            let repository = Repository::open(directory.path(), Path::new("kk")).await?;
            let snapshot = repository.snapshot(true).await?;
            let base = snapshot.exact_revision("@-").await?;
            let comparison = super::super::kk_comparison::Comparison::load(
                &snapshot,
                ComparisonIntent::for_target(base.revset(), "base".into(), None),
            )
            .await?;
            let mut queue = RepositoryQueue::default();
            let mut waiters = Vec::new();
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
                let (completion, completed) = oneshot::channel();
                let mut job = ClassificationJob::new(
                    snapshot.clone(),
                    comparison.diff_target(),
                    comparison.intent.clone(),
                    vec![FileSelection {
                        file: comparison.files[0].clone(),
                        ranges: None,
                    }],
                    destination,
                    completion,
                    Vec::new(),
                )?;
                job.source_category = source;
                queue.enqueue(job);
                waiters.push(completed);
            }

            let mut executed = 0;
            while let Some((queued, current)) = queue.next() {
                queued.check_dependencies()?;
                assert_eq!(queued.has_dependencies(), executed > 0);
                let RepositoryJob::Classification(mut job) = queued.job else {
                    anyhow::bail!("expected classification");
                };
                let original = job.snapshot.clone();
                let files = job.files.clone();
                let completions = std::mem::take(&mut job.completions);
                let result = job.execute_queued(current.clone(), files).await?;
                queue.advance(&current, &original, &result.snapshot);
                queue.complete(&queued.successes, true);
                for completion in completions {
                    assert!(completion.send(Ok(result.snapshot.clone())).is_ok());
                }
                executed += 1;
            }
            assert_eq!(executed, 3, "dependent category changes must not coalesce");
            for waiter in waiters {
                assert!(waiter.await?.is_ok());
            }
            let current = repository.snapshot(false).await?;
            let comparison =
                super::super::kk_comparison::Comparison::load(&current, comparison.intent).await?;
            assert_eq!(comparison.files.len(), 1);
            assert!(
                comparison
                    .categories
                    .iter()
                    .all(|category| category.files.is_empty())
            );
            assert_eq!(
                std::fs::read(directory.path().join("file.txt"))?,
                b"changed\n"
            );
            Ok(())
        })
    }

    #[test]
    fn queued_batches_coalesce_until_a_save_barrier_and_advance_old_selections() -> Result<()> {
        futures::executor::block_on(async {
            let directory = tempfile::tempdir()?;
            for arguments in [
                vec!["init", "."],
                vec!["describe", "-m", "base"],
                vec!["new", "-m", "source"],
            ] {
                run_kk(directory.path(), &arguments).await?;
            }

            std::fs::write(directory.path().join("file.txt"), "one\ntwo\nthree\n")?;
            let repository = Repository::open(directory.path(), Path::new("kk")).await?;
            let snapshot = repository.snapshot(true).await?;
            let base = snapshot
                .revisions(Some("@-"), 1)
                .await?
                .pop()
                .context("base")?;
            let comparison = super::super::kk_comparison::Comparison::load(
                &snapshot,
                super::super::kk_comparison::Comparison::new(&base).intent,
            )
            .await?;
            let file = comparison.files.first().context("file")?.clone();
            let mut queue = RepositoryQueue::default();
            let mut completions = Vec::new();
            for line in 1..=3 {
                if line == 3 {
                    std::fs::write(directory.path().join("file.txt"), "one\ntwo\nTHREE\n")?;
                    kk_workbench::record_saved_file(repository.root(), "file.txt")?;
                }

                let (sender, receiver) = oneshot::channel();
                completions.push(receiver);
                let start = queue.enqueue(ClassificationJob::new(
                    snapshot.clone(),
                    comparison.diff_target(),
                    comparison.intent.clone(),
                    vec![FileSelection {
                        file: file.clone(),
                        ranges: Some(vec![LineRange {
                            side: SelectionSide::Modified,
                            begin: line,
                            end: line,
                        }]),
                    }],
                    ClassificationTarget::Category(Category::Reviewed),
                    sender,
                    Vec::new(),
                )?);
                assert_eq!(start, line == 1);
            }

            let mut counts = Vec::new();
            while let Some((queued, current)) = queue.next() {
                queued.check_dependencies()?;
                let RepositoryJob::Classification(job) = queued.job else {
                    anyhow::bail!("expected classification")
                };
                counts.push(job.completions.len());
                let original = job.snapshot.clone();
                let previous = current.clone();
                let result = ClassificationJob::execute(
                    current,
                    job.snapshot,
                    job.target,
                    job.intent,
                    job.files,
                    job.category,
                    job.saved,
                )
                .await?;
                assert!(result.publication.is_some());
                queue.complete(&queued.successes, true);
                queue.advance(&previous, &original, &result.snapshot);
                for completion in job.completions {
                    assert!(completion.send(Ok(result.snapshot.clone())).is_ok());
                }
            }

            assert_eq!(counts, vec![2, 1]);
            for completed in completions {
                assert!(completed.await?.is_ok());
            }

            let final_snapshot = repository.snapshot(false).await?;
            let comparison =
                super::super::kk_comparison::Comparison::load(&final_snapshot, comparison.intent)
                    .await?;
            assert!(comparison.files.is_empty());
            assert_eq!(comparison.categories.len(), 1);
            assert_eq!(
                std::fs::read(directory.path().join("file.txt"))?,
                b"one\ntwo\nTHREE\n"
            );
            let (sender, completed) = oneshot::channel();
            queue.enqueue(ClassificationJob::new(
                final_snapshot.clone(),
                comparison.diff_target(),
                comparison.intent,
                vec![FileSelection { file, ranges: None }],
                ClassificationTarget::Category(Category::Rejected),
                sender,
                Vec::new(),
            )?);
            drop(queue);
            let cancelled = completed
                .await?
                .err()
                .context("queued work should be cancelled on close")?;
            assert!(!cancelled.uncertain);
            assert_eq!(
                repository.snapshot(false).await?.operation(),
                final_snapshot.operation()
            );
            Ok(())
        })
    }
}
