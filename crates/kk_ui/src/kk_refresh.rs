use super::*;
use db::kvp::KeyValueStore;
use futures::{StreamExt as _, channel::mpsc};
use kk_workbench::{Diagnostics, PerformanceContext, Repository};
use std::sync::Arc;

pub(super) const CONCURRENCY: usize = 4;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum RefreshPhase {
    Queued,
    Opening,
    CheckingOperation,
    ReadingWorkspace,
    ReadingHistory,
    ReadingFiles(String),
    UpdatingComparison(String),
}

impl RefreshPhase {
    pub fn label(&self) -> String {
        match self {
            Self::Queued => "Waiting to refresh…".into(),
            Self::Opening => "Opening repository…".into(),
            Self::CheckingOperation => "Checking previous operation…".into(),
            Self::ReadingWorkspace => "Reading workspace state…".into(),
            Self::ReadingHistory => "Reading commit history…".into(),
            Self::ReadingFiles(change) => format!("Reading changed files for {change}…"),
            Self::UpdatingComparison(target) => format!("Updating comparison with {target}…"),
        }
    }
}

pub(super) enum RefreshEvent {
    Scanning {
        worktree: EntityId,
        path: PathBuf,
    },
    Phase {
        root: PathBuf,
        phase: RefreshPhase,
    },
    Discovered {
        worktree: EntityId,
        path: PathBuf,
        roots: Vec<PathBuf>,
        errors: Vec<String>,
    },
    Loaded(Box<RepositoryRefresh>),
}

pub(super) struct RepositoryRefresh {
    pub root: PathBuf,
    pub view: anyhow::Result<RepositoryView>,
    pub errors: Vec<String>,
    pub settled: Vec<(PathBuf, bool, Snapshot)>,
    pub loaded_comparisons: HashSet<PathBuf>,
}

pub(super) struct RefreshRequest {
    pub paths: Vec<(EntityId, PathBuf)>,
    pub remote: Option<kk_workbench::remote::Client>,
    pub executable: PathBuf,
    pub previous: Vec<RepositoryView>,
    pub mode: HistoryMode,
    pub history_limits: HashMap<PathBuf, usize>,
    pub uncertain: HashMap<PathBuf, OperationReceipt>,
    pub comparison_store: Arc<KeyValueStore>,
    pub comparison_host: Option<String>,
    pub diagnostics: Diagnostics,
    pub performance_context: Option<PerformanceContext>,
}

impl RefreshRequest {
    pub async fn run(self, output: mpsc::UnboundedSender<RefreshEvent>) {
        let (jobs, pending) = mpsc::unbounded();
        let request = &self;
        let discover = {
            let jobs = jobs.clone();
            let output = output.clone();
            async move {
                let mut scheduled = HashSet::new();
                for previous in &request.previous {
                    let repository = previous.snapshot.repository();
                    if scheduled.insert(repository.root().to_owned())
                        && jobs.unbounded_send((repository.clone(), true)).is_err()
                    {
                        return;
                    }
                }

                let mut discoveries = futures::stream::iter(request.paths.iter().cloned())
                    .map(|(worktree, path)| {
                        let output = &output;
                        async move {
                            output
                                .unbounded_send(RefreshEvent::Scanning {
                                    worktree,
                                    path: path.clone(),
                                })
                                .ok()?;
                            let discovery = if let Some(remote) = &request.remote {
                                remote
                                    .discover(
                                        &path,
                                        &request.executable,
                                        request.diagnostics.clone(),
                                    )
                                    .await
                            } else {
                                Ok(discover_with_diagnostics(
                                    &path,
                                    &request.executable,
                                    request.diagnostics.clone(),
                                    request.performance_context.clone(),
                                )
                                .await)
                            };
                            Some((worktree, path, discovery))
                        }
                    })
                    .buffer_unordered(CONCURRENCY);
                while let Some(Some((worktree, path, discovery))) = discoveries.next().await {
                    let (repositories, errors) = match discovery {
                        Ok(discovery) => (
                            discovery.repositories,
                            discovery
                                .errors
                                .into_iter()
                                .map(|error| format!("{}: {}", error.path.display(), error.message))
                                .collect(),
                        ),
                        Err(error) => (Vec::new(), vec![format!("{}: {error:#}", path.display())]),
                    };
                    let roots = repositories
                        .iter()
                        .map(|repository| repository.root().to_owned())
                        .collect();
                    if output
                        .unbounded_send(RefreshEvent::Discovered {
                            worktree,
                            path,
                            roots,
                            errors,
                        })
                        .is_err()
                    {
                        return;
                    }
                    for repository in repositories {
                        if scheduled.insert(repository.root().to_owned())
                            && jobs.unbounded_send((repository, false)).is_err()
                        {
                            return;
                        }
                    }
                }
            }
        };
        drop(jobs);
        let load = async {
            let mut loads = pending
                .map(|(repository, reopen)| self.load(repository, reopen, &output))
                .buffer_unordered(CONCURRENCY);
            while let Some(event) = loads.next().await {
                if output.unbounded_send(event).is_err() {
                    return;
                }
            }
        };
        futures::join!(discover, load);
    }

    pub(super) async fn load(
        &self,
        repository: Repository,
        reopen: bool,
        output: &mpsc::UnboundedSender<RefreshEvent>,
    ) -> RefreshEvent {
        let root = repository.root().to_owned();
        let report = |phase| {
            // The receiver can be dropped when a newer refresh replaces this one.
            drop(output.unbounded_send(RefreshEvent::Phase {
                root: root.clone(),
                phase,
            }));
        };
        let repository = if reopen
            && (repository.is_remote()
                || !repository.matches_source(&self.executable, self.remote.as_ref()))
        {
            report(RefreshPhase::Opening);
            let opened = if let Some(remote) = &self.remote {
                remote
                    .open(&root, &self.executable, self.diagnostics.clone())
                    .await
            } else {
                Repository::open_with_diagnostics(
                    &root,
                    &self.executable,
                    self.diagnostics.clone(),
                    self.performance_context.clone(),
                )
                .await
            };
            match opened {
                Ok(repository) => repository,
                Err(error) => {
                    return RefreshEvent::Loaded(Box::new(RepositoryRefresh {
                        root,
                        view: Err(error),
                        errors: Vec::new(),
                        settled: Vec::new(),
                        loaded_comparisons: HashSet::new(),
                    }));
                }
            }
        } else {
            repository
        };
        let mut errors = Vec::new();
        let mut settled = Vec::new();
        let mut loaded_comparisons = HashSet::new();
        let previous = self
            .previous
            .iter()
            .find(|view| view.snapshot.repository().root() == repository.root());
        let mode = self.mode;
        let history_limits = &self.history_limits;
        let comparison_store = &self.comparison_store;
        let comparison_host = &self.comparison_host;
        let diagnostics = &self.diagnostics;
        if let Some(receipt) = self.uncertain.get(&root) {
            report(RefreshPhase::CheckingOperation);
            match receipt.with_repository(&repository) {
                Ok(receipt) => {
                    let status = async {
                        match receipt.status().await? {
                            PublicationStatus::Applied => {
                                Ok(Some((true, receipt.synchronize().await?)))
                            }
                            PublicationStatus::Rejected => Ok(Some((
                                false,
                                receipt.previous.repository().snapshot(false).await?,
                            ))),
                            PublicationStatus::Unknown => anyhow::Ok(None),
                        }
                    }
                    .await;
                    match status {
                        Ok(Some((applied, snapshot))) => {
                            settled.push((root.clone(), applied, snapshot))
                        }
                        Ok(None) => {}
                        Err(error) => errors.push(format!("{}: {error:#}", root.display())),
                    }
                }
                Err(error) => errors.push(format!("{}: {error:#}", root.display())),
            }
        }
        let loaded = async {
            report(RefreshPhase::ReadingWorkspace);
            let snapshot = match repository.snapshot(true).await {
                Ok(snapshot) => snapshot,
                Err(error) if kk_workbench::is_stale_workspace(&error) => {
                    diagnostics.error(format!("{}: {error:#}", repository.root().display()));
                    let snapshot = repository.snapshot(false).await?;
                    let comparison = match previous {
                        Some(previous) => previous.comparison.clone(),
                        None => comparison_store
                            .read_kvp(&ComparisonState::key(
                                repository.root(),
                                comparison_host.as_deref(),
                            ))?
                            .and_then(|text| ComparisonIntent::decode(&text))
                            .map(Comparison::pending),
                    };
                    return anyhow::Ok(RepositoryView {
                        history_snapshot: previous.map(|previous| {
                            previous
                                .history_snapshot
                                .as_ref()
                                .unwrap_or(&previous.snapshot)
                                .clone()
                        }),
                        stale: true,
                        mode: previous.map(|previous| previous.mode).unwrap_or(mode),
                        snapshot,
                        history: previous.map(|previous| previous.history.clone()).unwrap_or(
                            HistoryPage {
                                revisions: Vec::new(),
                                has_more: false,
                            },
                        ),
                        expanded: previous
                            .map(|previous| previous.expanded.clone())
                            .unwrap_or_default(),
                        comparison,
                        history_targets: previous
                            .map(|previous| previous.history_targets.clone())
                            .unwrap_or_default(),
                    });
                }
                Err(error) => return Err(error),
            };
            let snapshot = if let Some(lifetime) = previous
                .and_then(|view| view.snapshot.repository().lifetime())
                .filter(|lifetime| lifetime.check().is_ok())
            {
                snapshot.with_lifetime(Some(lifetime.clone()))
            } else {
                snapshot
            };
            let limit = history_limits.get(repository.root()).copied().unwrap_or(50);
            let targets = if previous.is_some_and(|previous| previous.mode != mode) {
                &[][..]
            } else {
                previous
                    .map(|view| view.history_targets.as_slice())
                    .unwrap_or_default()
            };
            report(RefreshPhase::ReadingHistory);
            let (history, history_targets) =
                snapshot.history_containing(mode, limit, targets).await?;
            let mut expanded = HashMap::new();
            let mut comparison = None;
            if let Some(previous) = previous {
                for revision in history.revisions.iter() {
                    let was_expanded = previous
                        .history
                        .revisions
                        .iter()
                        .chain(previous.history_targets.iter())
                        .any(|old| {
                            previous.expanded.contains_key(&old.full_commit_id)
                                && (old.full_commit_id == revision.full_commit_id
                                    || (old.full_change_id == revision.full_change_id
                                        && history
                                            .revisions
                                            .iter()
                                            .filter(|candidate| {
                                                candidate.full_change_id == revision.full_change_id
                                            })
                                            .take(2)
                                            .count()
                                            == 1))
                        });
                    if was_expanded {
                        report(RefreshPhase::ReadingFiles(revision.change_id.clone()));
                        let files = match snapshot
                            .changed_files(&DiffScope::Change(revision.full_commit_id.clone()))
                            .await
                        {
                            Ok(files) => RevisionFiles::Loaded(files),
                            Err(error) => RevisionFiles::Failed(format!("{error:#}")),
                        };
                        expanded.insert(revision.full_commit_id.clone(), files);
                    }
                }
            }

            let intent = match previous {
                Some(previous) => previous
                    .comparison
                    .as_ref()
                    .map(|comparison| comparison.intent.clone()),
                None => comparison_store
                    .read_kvp(&ComparisonState::key(
                        repository.root(),
                        comparison_host.as_deref(),
                    ))?
                    .and_then(|text| ComparisonIntent::decode(&text)),
            };
            if let Some(intent) = intent {
                report(RefreshPhase::UpdatingComparison(
                    intent.target_label.clone(),
                ));
                match Comparison::load(&snapshot, intent.clone()).await {
                    Ok(loaded) => {
                        loaded_comparisons.insert(repository.root().to_owned());
                        comparison = Some(loaded);
                    }
                    Err(error) => {
                        errors.push(format!("{}: {error:#}", repository.root().display()));
                        comparison = previous
                            .and_then(|view| view.comparison.clone())
                            .or_else(|| Some(Comparison::pending(intent)));
                    }
                }
            }

            anyhow::Ok(RepositoryView {
                history_snapshot: None,
                stale: false,
                mode,
                snapshot,
                history,
                expanded,
                comparison,
                history_targets,
            })
        }
        .await;
        RefreshEvent::Loaded(Box::new(RepositoryRefresh {
            root,
            view: loaded,
            errors,
            settled,
            loaded_comparisons,
        }))
    }
}

#[cfg(test)]
#[path = "kk_refresh_tests.rs"]
mod tests;
