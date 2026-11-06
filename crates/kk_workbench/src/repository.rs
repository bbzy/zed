use crate::history::{HISTORY_PAGE_SIZE, HistoryMode, HistoryPage, Revision};
use crate::{Diagnostics, PerformanceContext};
use anyhow::{Context as _, Result, bail, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use smol::process::Command;
use std::{
    collections::HashMap,
    path::{Component, Path, PathBuf},
    process::{Output, Stdio},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

const FILE_TEMPLATE: &str = r#""{\"path\":" ++ json(path) ++ ",\"changeType\":" ++ json(status_char) ++ ",\"renamedFrom\":" ++ if(status_char == "R" || status_char == "C", json(source.path()), "null") ++ "}\n""#;

pub fn is_stale_workspace(error: &anyhow::Error) -> bool {
    format!("{error:#}").contains("workspace update-stale")
}

#[derive(Clone, Debug)]
pub struct Repository {
    root: Arc<Path>,
    executable: Arc<Path>,
    diagnostics: Diagnostics,
    performance_context: Option<PerformanceContext>,
    lifetime: Option<RepositoryLifetime>,
    remote: Option<Arc<crate::remote_client::RemoteRepository>>,
}

#[derive(Clone, Debug)]
pub struct RepositoryLifetime(Arc<LifetimeState>);

#[derive(Debug)]
struct LifetimeState {
    active: AtomicBool,
    id: u128,
    openings: Arc<crate::opening::DiffOpenings>,
    closed: smol::channel::Receiver<()>,
    close: Mutex<Option<smol::channel::Sender<()>>>,
    remote: Mutex<Vec<Weak<crate::remote_client::RemoteRepository>>>,
}

impl Default for RepositoryLifetime {
    fn default() -> Self {
        Self::new(Arc::default())
    }
}

impl PartialEq for RepositoryLifetime {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for RepositoryLifetime {}

impl std::hash::Hash for RepositoryLifetime {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::hash::Hash::hash(&Arc::as_ptr(&self.0), state);
    }
}

impl RepositoryLifetime {
    fn new(openings: Arc<crate::opening::DiffOpenings>) -> Self {
        let (close, closed) = smol::channel::bounded(1);
        Self(Arc::new(LifetimeState {
            active: AtomicBool::new(true),
            id: uuid::Uuid::new_v4().as_u128(),
            openings,
            closed,
            close: Mutex::new(Some(close)),
            remote: Mutex::new(Vec::new()),
        }))
    }

    pub(crate) fn sharing_openings(&self) -> Self {
        Self::new(self.0.openings.clone())
    }

    pub fn invalidate(&self) {
        self.0.active.store(false, Ordering::Release);
        self.0
            .close
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        self.0.openings.cancel(self.0.id);
        let remote = self
            .0
            .remote
            .lock()
            .unwrap_or_else(|error| {
                log::error!("Remote repository lifetime lock was poisoned: {error}");
                error.into_inner()
            })
            .drain(..)
            .filter_map(|remote| remote.upgrade())
            .collect::<Vec<_>>();
        for remote in remote {
            remote.detach();
        }
    }

    fn bind_remote(&self, remote: &Arc<crate::remote_client::RemoteRepository>) {
        let mut bound = self.0.remote.lock().unwrap_or_else(|error| {
            log::error!("Remote repository lifetime lock was poisoned: {error}");
            error.into_inner()
        });
        if !self.0.active.load(Ordering::Acquire) {
            drop(bound);
            remote.detach();
        } else {
            bound.retain(|remote| remote.strong_count() > 0);
            let remote = Arc::downgrade(remote);
            if !bound.iter().any(|known| known.ptr_eq(&remote)) {
                bound.push(remote);
            }
        }
    }

    pub fn check(&self) -> Result<()> {
        ensure!(
            self.0.active.load(Ordering::Acquire),
            "The selected repository was closed. Nothing was published. Reopen it and select the changes again."
        );
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    repository: Repository,
    operation: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ComparisonBoundary {
    pub base_revset: String,
    pub range_start: Option<Revision>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum ChangeKind {
    #[serde(rename = "M")]
    Modified,
    #[serde(rename = "A")]
    Added,
    #[serde(rename = "D")]
    Deleted,
    #[serde(rename = "R")]
    Renamed,
    #[serde(rename = "C")]
    Copied,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ChangedFile {
    pub path: String,
    pub change_type: ChangeKind,
    pub renamed_from: Option<String>,
    #[serde(default)]
    pub has_conflict: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum DiffScope {
    Change(String),
    Comparison { base: String, target: String },
}

impl DiffScope {
    fn args(&self) -> Vec<String> {
        match self {
            Self::Change(revision) => vec!["-r".into(), revision.clone()],
            Self::Comparison { base, target } => {
                vec!["--from".into(), base.clone(), "--to".into(), target.clone()]
            }
        }
    }
}

impl Repository {
    pub async fn open(directory: &Path, executable: &Path) -> Result<Self> {
        Self::open_with_diagnostics(directory, executable, Diagnostics::default(), None).await
    }

    pub async fn open_with_diagnostics(
        directory: &Path,
        executable: &Path,
        diagnostics: Diagnostics,
        performance_context: Option<PerformanceContext>,
    ) -> Result<Self> {
        let output = run_output(
            executable,
            directory,
            &["root", "--ignore-working-copy"],
            &diagnostics,
            performance_context.as_ref(),
        )
        .await?
        .stdout;
        let root = PathBuf::from(std::str::from_utf8(&output)?.trim_end_matches(['\r', '\n']));
        ensure!(
            root.is_absolute(),
            "KK returned a non-absolute repository root"
        );
        let marker = root.join(".kk");
        ensure!(
            smol::fs::metadata(&marker).await.is_ok(),
            "KK metadata is missing at {}",
            marker.display()
        );
        let root = smol::fs::canonicalize(root).await?;

        Ok(Self {
            root: root.into(),
            executable: executable.to_owned().into(),
            diagnostics,
            performance_context,
            lifetime: None,
            remote: None,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn from_remote(
        client: crate::remote::Client,
        info: crate::remote::RepositoryInfo,
        executable: &Path,
        diagnostics: Diagnostics,
    ) -> Self {
        Self {
            root: info.root.into(),
            executable: executable.to_owned().into(),
            diagnostics,
            performance_context: None,
            lifetime: None,
            remote: Some(Arc::new(crate::remote_client::RemoteRepository {
                client,
                id: info.id,
            })),
        }
    }

    pub fn is_remote(&self) -> bool {
        self.remote.is_some()
    }

    pub(crate) fn remote(&self) -> Option<&crate::remote_client::RemoteRepository> {
        self.remote.as_deref()
    }

    pub(crate) fn same_location(&self, other: &Self) -> bool {
        match (&self.remote, &other.remote) {
            (Some(first), Some(second)) => {
                self.root == other.root && first.client.same_connection(&second.client)
            }
            (None, None) => self.root == other.root,
            _ => false,
        }
    }

    pub(crate) fn with_lifetime(&self, lifetime: RepositoryLifetime) -> Self {
        if let Some(remote) = &self.remote {
            lifetime.bind_remote(remote);
        }

        Self {
            lifetime: Some(lifetime),
            ..self.clone()
        }
    }

    pub fn diagnostics(&self) -> &Diagnostics {
        &self.diagnostics
    }

    pub fn lifetime(&self) -> Option<&RepositoryLifetime> {
        self.lifetime.as_ref()
    }

    pub fn check_lifetime(&self) -> Result<()> {
        if let Some(lifetime) = &self.lifetime {
            lifetime.check()?;
        }

        Ok(())
    }

    pub fn with_performance_context(&self, context: Option<PerformanceContext>) -> Self {
        Self {
            performance_context: context,
            ..self.clone()
        }
    }

    pub fn performance_context(&self) -> Option<&PerformanceContext> {
        self.performance_context.as_ref()
    }

    pub async fn begin_diff_opening(&self) -> Result<crate::DiffOpening> {
        self.check_lifetime()?;
        if let Some(remote) = self.remote() {
            let id = uuid::Uuid::new_v4().as_u128();
            let opening = crate::DiffOpening::remote(remote.client.clone(), id);
            ensure!(
                matches!(
                    remote
                        .client
                        .request(crate::remote::Request::BeginDiffOpening {
                            repository: remote.id,
                            id,
                        })
                        .await?,
                    crate::remote::Response::Done
                ),
                "Expected diff-opening acknowledgement"
            );
            return Ok(opening);
        }

        Ok(match &self.lifetime {
            Some(lifetime) => lifetime.0.openings.begin(lifetime.0.id),
            None => crate::opening::DiffOpenings::default().begin(0),
        })
    }

    pub(crate) async fn yield_to_diff_opening(&self) -> Result<()> {
        if let Some(lifetime) = &self.lifetime {
            if !lifetime.0.openings.has_pending() {
                return lifetime.check();
            }

            let span = self.diagnostics.start(
                "split.waitForDiffOpening",
                self.performance_context.as_ref(),
                serde_json::Value::Null,
            );
            let wait = lifetime.0.openings.wait();
            let closed = lifetime.0.closed.recv();
            match futures::future::select(std::pin::pin!(wait), std::pin::pin!(closed)).await {
                futures::future::Either::Left((result, _)) => result?,
                futures::future::Either::Right(_) => lifetime.check()?,
            }
            lifetime.check()?;
            span.finish("completed", serde_json::Value::Null);
        }

        Ok(())
    }

    pub async fn relative_file_path(&self, absolute: &Path) -> Result<String> {
        if let Some(remote) = self.remote() {
            return remote
                .data(crate::remote::Request::RelativeFilePath {
                    repository: remote.id,
                    path: absolute.to_owned(),
                })
                .await;
        }

        let parent = absolute.parent().context("File has no parent directory")?;
        let path = smol::fs::canonicalize(parent)
            .await?
            .join(absolute.file_name().context("File has no name")?);
        let path = path
            .strip_prefix(self.root())
            .context("File is outside the repository")?
            .to_str()
            .context("File path is not UTF-8")?
            .to_owned();
        validate_path(&path)?;
        Ok(path)
    }

    pub fn snapshot_at(&self, operation: String) -> Result<Snapshot> {
        self.check_lifetime()?;
        Snapshot::from_operation(self.clone(), operation)
    }

    pub async fn snapshot(&self, capture_working_copy: bool) -> Result<Snapshot> {
        if let Some(remote) = self.remote() {
            if capture_working_copy {
                self.check_lifetime()?;
            }

            return remote
                .snapshot_response(
                    self,
                    crate::remote::Request::Snapshot {
                        repository: remote.id,
                        capture: capture_working_copy,
                    },
                )
                .await;
        }

        let _write_guard = if capture_working_copy {
            self.check_lifetime()?;
            let guard = crate::transaction::write_lock(self.root()).await?;
            self.check_lifetime()?;
            Some(guard)
        } else {
            None
        };
        let capture_working_copy = capture_working_copy && !crate::saved::is_deferred(self.root())?;
        self.snapshot_unlocked(capture_working_copy).await
    }

    pub async fn snapshot_saved_working_copy(&self) -> Result<Snapshot> {
        self.check_lifetime()?;
        if let Some(remote) = self.remote() {
            return remote
                .snapshot_response(
                    self,
                    crate::remote::Request::SnapshotSavedWorkingCopy {
                        repository: remote.id,
                    },
                )
                .await;
        }

        let _write_guard = crate::transaction::write_lock(self.root()).await?;
        self.check_lifetime()?;
        ensure!(
            !crate::saved::is_deferred(self.root())?,
            "Saved contents are deferred by a pending classification. Finish or cancel it and try again."
        );
        self.snapshot_unlocked(true).await
    }

    pub async fn snapshot_before_write(&self) -> Result<Snapshot> {
        self.check_lifetime()?;
        if let Some(remote) = self.remote() {
            return remote
                .snapshot_response(
                    self,
                    crate::remote::Request::SnapshotBeforeWrite {
                        repository: remote.id,
                    },
                )
                .await;
        }

        let _write_guard = crate::transaction::write_lock(self.root()).await?;
        self.check_lifetime()?;
        self.snapshot_unlocked(true).await
    }

    pub async fn update_stale_workspace(&self) -> Result<Snapshot> {
        self.check_lifetime()?;
        if let Some(remote) = self.remote() {
            return remote
                .snapshot_response(
                    self,
                    crate::remote::Request::UpdateStaleWorkspace {
                        repository: remote.id,
                    },
                )
                .await;
        }

        let _guard = crate::transaction::write_lock(self.root()).await?;
        self.check_lifetime()?;
        ensure!(
            !crate::saved::is_deferred(self.root())?,
            "Wait for pending saved-file classifications before updating the workspace"
        );
        self.command(&["workspace", "update-stale"]).await?;
        self.snapshot_unlocked(false).await
    }

    pub(crate) async fn snapshot_unlocked(&self, capture_working_copy: bool) -> Result<Snapshot> {
        if capture_working_copy {
            self.command(&["util", "snapshot"]).await?;
        }

        let output = self
            .command(&[
                "op",
                "log",
                "--limit",
                "1",
                "--no-graph",
                "-T",
                "id",
                "--ignore-working-copy",
            ])
            .await?
            .stdout;
        let operation = String::from_utf8(output)?.trim().to_owned();
        Snapshot::from_operation(self.clone(), operation)
            .context("KK returned an invalid operation ID")
    }

    pub(crate) async fn command(&self, arguments: &[&str]) -> Result<Output> {
        ensure!(
            self.remote.is_none(),
            "Remote KK commands must execute on the server"
        );
        run_output(
            &self.executable,
            &self.root,
            arguments,
            &self.diagnostics,
            self.performance_context.as_ref(),
        )
        .await
    }
}

impl Snapshot {
    pub(crate) fn from_operation(repository: Repository, operation: String) -> Result<Self> {
        ensure!(
            !operation.is_empty() && operation.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "Invalid operation ID"
        );
        Ok(Self {
            repository,
            operation,
        })
    }

    pub fn with_lifetime(&self, lifetime: Option<RepositoryLifetime>) -> Self {
        let mut snapshot = self.clone();
        if let Some(lifetime) = &lifetime
            && let Some(remote) = &snapshot.repository.remote
        {
            lifetime.bind_remote(remote);
        }

        snapshot.repository.lifetime = lifetime;
        snapshot
    }

    pub fn with_performance_context(&self, context: Option<PerformanceContext>) -> Self {
        Self {
            repository: self.repository.with_performance_context(context),
            ..self.clone()
        }
    }

    pub(crate) fn at_operation(&self, operation: String) -> Self {
        Self {
            repository: self.repository.clone(),
            operation,
        }
    }

    pub async fn current_file_path(&self, revision: &str, path: &str) -> Result<Option<String>> {
        if let Some(remote) = self.repository().remote() {
            return remote
                .read(
                    self,
                    crate::remote::Read::CurrentFilePath {
                        revision: revision.into(),
                        path: path.into(),
                    },
                )
                .await;
        }

        validate_path(path)?;
        let exists = |path: PathBuf| async move {
            match smol::fs::metadata(path).await {
                Ok(_) => Ok(true),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) =>
                {
                    Ok(false)
                }
                Err(error) => Err(error),
            }
        };
        if exists(self.repository.root().join(path)).await? {
            return Ok(Some(path.into()));
        }

        if revision != "@" {
            let files = self
                .changed_files(&DiffScope::Comparison {
                    base: revision.into(),
                    target: "@".into(),
                })
                .await?;
            if let Some(file) = files
                .into_iter()
                .find(|file| file.renamed_from.as_deref() == Some(path))
                && exists(self.repository.root().join(&file.path)).await?
            {
                return Ok(Some(file.path));
            }
        }

        Ok(None)
    }

    pub fn operation(&self) -> &str {
        &self.operation
    }

    pub fn repository(&self) -> &Repository {
        &self.repository
    }

    pub(crate) async fn read(&self, arguments: &[String]) -> Result<Vec<u8>> {
        let mut command_arguments =
            vec!["--at-operation", &self.operation, "--ignore-working-copy"];
        command_arguments.extend(arguments.iter().map(String::as_str));

        Ok(self.repository.command(&command_arguments).await?.stdout)
    }

    pub async fn revisions(&self, revset: Option<&str>, limit: usize) -> Result<Vec<Revision>> {
        if let Some(remote) = self.repository().remote() {
            return remote
                .read(
                    self,
                    crate::remote::Read::Revisions {
                        revset: revset.map(str::to_owned),
                        limit,
                    },
                )
                .await;
        }

        let mut arguments = vec![
            "log".into(),
            "--no-graph".into(),
            "--limit".into(),
            limit.to_string(),
            "-T".into(),
            LOG_TEMPLATE.into(),
        ];
        if let Some(revset) = revset {
            arguments.extend(["-r".into(), revset.into()]);
        }

        parse_records(&self.read(&arguments).await?)
    }

    pub async fn history(&self, mode: HistoryMode, visible_limit: usize) -> Result<HistoryPage> {
        if let Some(remote) = self.repository().remote() {
            return remote
                .read(
                    self,
                    crate::remote::Read::History {
                        mode,
                        limit: visible_limit,
                    },
                )
                .await;
        }

        let visible_limit = visible_limit.max(HISTORY_PAGE_SIZE);
        let revset = (mode == HistoryMode::Lineage).then_some("first_ancestors(@)");
        let mut revisions = self
            .revisions(revset, visible_limit.saturating_add(1))
            .await?;
        let has_more = revisions.len() > visible_limit;
        revisions.truncate(visible_limit);

        Ok(HistoryPage {
            revisions,
            has_more,
        })
    }

    pub async fn commit_emptiness(&self, commit_ids: &[String]) -> Result<HashMap<String, bool>> {
        if let Some(remote) = self.repository().remote() {
            return remote
                .read(
                    self,
                    crate::remote::Read::CommitEmptiness {
                        commits: commit_ids.to_vec(),
                    },
                )
                .await;
        }

        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Entry {
            full_commit_id: String,
            is_empty: bool,
        }

        let mut results = HashMap::new();
        for commits in commit_ids.chunks(128) {
            let revset = commits
                .iter()
                .map(|commit| Ok(format!("commit_id({})", serde_json::to_string(commit)?)))
                .collect::<Result<Vec<_>>>()?
                .join(" | ");
            let output = self.read(&[
                "log".into(), "--no-graph".into(), "-r".into(), revset, "-T".into(),
                r#""\x1e{\"fullCommitId\":" ++ json(commit_id) ++ ",\"isEmpty\":" ++ if(empty, "true", "false") ++ "}\n""#.into(),
            ]).await?;
            for entry in parse_records::<Entry>(&output)? {
                ensure!(
                    commits.contains(&entry.full_commit_id),
                    "Unexpected commit in empty-change metadata"
                );
                results.insert(entry.full_commit_id, entry.is_empty);
            }

            ensure!(
                commits.iter().all(|commit| results.contains_key(commit)),
                "Incomplete empty-change metadata"
            );
        }

        Ok(results)
    }

    pub async fn comparison_candidates(&self) -> Result<Vec<Revision>> {
        if let Some(remote) = self.repository().remote() {
            return remote
                .read(self, crate::remote::Read::ComparisonCandidates)
                .await;
        }

        self.revisions(Some("(ancestors(@, 50) | mutable() | bookmarks() | remote_bookmarks() | tags() | root()) ~ @"), 50).await
    }

    pub async fn resolve_revision(&self, previous: &Revision) -> Result<Revision> {
        if let Some(remote) = self.repository().remote() {
            return remote
                .read(
                    self,
                    crate::remote::Read::ResolveRevision {
                        revision: previous.clone(),
                    },
                )
                .await;
        }

        let mut revisions = self.revisions(Some(&previous.revset()), 3).await?;
        if let Some(index) = revisions
            .iter()
            .position(|revision| revision.full_commit_id == previous.full_commit_id)
        {
            return Ok(revisions.remove(index));
        }

        ensure!(revisions.len() == 1, "Change no longer resolves uniquely");
        revisions.pop().context("Change is no longer available")
    }

    pub async fn history_containing(
        &self,
        mode: HistoryMode,
        visible_limit: usize,
        revision: &Revision,
    ) -> Result<(HistoryPage, Option<Revision>)> {
        if let Some(remote) = self.repository().remote() {
            return remote
                .read(
                    self,
                    crate::remote::Read::HistoryContaining {
                        mode,
                        limit: visible_limit,
                        revision: revision.clone(),
                    },
                )
                .await;
        }

        let revision = self.resolve_revision(revision).await?;
        let mut limit = visible_limit.max(HISTORY_PAGE_SIZE);
        let mut history = self.history(mode, limit).await?;
        let outside_lineage = mode == HistoryMode::Lineage
            && self
                .revisions(
                    Some(&format!("{} & first_ancestors(@)", revision.full_commit_id)),
                    1,
                )
                .await?
                .is_empty();
        loop {
            if history
                .revisions
                .iter()
                .any(|candidate| candidate.full_commit_id == revision.full_commit_id)
            {
                return Ok((history, None));
            }

            if outside_lineage || !history.has_more {
                return Ok((history, Some(revision)));
            }

            limit = limit
                .checked_add(HISTORY_PAGE_SIZE)
                .context("History limit overflow")?;
            history = self.history(mode, limit).await?;
        }
    }

    pub async fn search_revisions(
        &self,
        query: &str,
        scope: Option<&str>,
        include_working_copy: bool,
        limit: usize,
    ) -> Result<Vec<Revision>> {
        if let Some(remote) = self.repository().remote() {
            return remote
                .read(
                    self,
                    crate::remote::Read::SearchRevisions {
                        query: query.into(),
                        scope: scope.map(str::to_owned),
                        working_copy: include_working_copy,
                        limit,
                    },
                )
                .await;
        }

        let query = query.trim();
        if query.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }

        let literal = serde_json::to_string(query)?;
        let mut exact = Vec::new();
        if query.bytes().all(|byte| (b'k'..=b'z').contains(&byte)) {
            exact.push(format!("change_id({literal})"));
        }

        if query
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            exact.push(format!("commit_id({literal})"));
        }

        for field in [
            "subject",
            "description",
            "bookmarks",
            "remote_bookmarks",
            "tags",
        ] {
            exact.push(format!("{field}(exact-i:{literal})"));
        }

        let exact = exact.join(" | ");
        let prefix_literal = serde_json::to_string(&format!("^{}", regex::escape(query)))?;
        let prefix = ["subject", "bookmarks", "remote_bookmarks", "tags"]
            .map(|field| format!("{field}(regex-i:{prefix_literal})"))
            .join(" | ");
        let substring = ["description", "bookmarks", "remote_bookmarks", "tags"]
            .map(|field| format!("{field}(substring-i:{literal})"))
            .join(" | ");
        let phases = [
            exact.clone(),
            format!("({prefix}) ~ ({exact})"),
            format!("({substring}) ~ ({exact} | {prefix})"),
        ];
        let mut matches = Vec::new();
        for phase in phases {
            if matches.len() == limit {
                break;
            }

            let mut revset = match scope {
                Some(scope) => format!("({scope}) & ({phase})"),
                None => phase,
            };
            if !include_working_copy {
                revset = format!("({revset}) ~ @");
            }

            matches.extend(self.revisions(Some(&revset), limit - matches.len()).await?);
        }

        Ok(crate::history::rank_revisions(matches, query))
    }

    pub async fn comparison_boundary(
        &self,
        target: &str,
        range_start: Option<&str>,
    ) -> Result<ComparisonBoundary> {
        if let Some(remote) = self.repository().remote() {
            return remote
                .read(
                    self,
                    crate::remote::Read::ComparisonBoundary {
                        target: target.into(),
                        range_start: range_start.map(str::to_owned),
                    },
                )
                .await;
        }

        let range_start = if let Some(change_id) = range_start {
            ensure!(
                !change_id.is_empty()
                    && change_id.bytes().all(|byte| (b'k'..=b'z').contains(&byte)),
                "Invalid comparison change ID"
            );
            let mut revisions = self
                .revisions(Some(&format!("present(change_id({change_id})) & ::@")), 2)
                .await?;
            ensure!(
                revisions.len() == 1,
                "Comparison range start no longer resolves uniquely on the working-copy ancestry"
            );
            let revision = revisions.pop().context("Missing comparison range start")?;
            ensure!(
                revision.parent_commit_ids.len() == 1,
                "Comparison range start must have one parent"
            );
            Some(revision)
        } else {
            let mut revisions = self
                .revisions(Some(&format!("exactly(({target}), 1)")), 2)
                .await?;
            let mut cursor = revisions.pop().context("Comparison target is missing")?;
            let mut visited = std::collections::HashSet::new();
            loop {
                if !visited.insert(cursor.full_commit_id.clone()) {
                    bail!("Cycle in comparison history");
                }

                let mut children = self
                    .revisions(
                        Some(&format!("children({}) & ::@", cursor.full_commit_id)),
                        2,
                    )
                    .await?;
                if children.len() != 1 {
                    break None;
                }

                cursor = children.pop().context("Missing comparison child")?;
                if cursor.parent_commit_ids.len() != 1 {
                    break None;
                }

                if !matches!(
                    cursor.full_description.trim_end(),
                    "KKW: REVIEWED" | "KKW: REJECTED" | "KKW: DEBUG"
                ) {
                    break Some(cursor);
                }
            }
        };
        let base_revset = match &range_start {
            Some(revision) => format!("exactly(parents({}), 1)", revision.revset()),
            None => target.to_owned(),
        };

        Ok(ComparisonBoundary {
            base_revset,
            range_start,
        })
    }

    pub async fn changed_files(&self, scope: &DiffScope) -> Result<Vec<ChangedFile>> {
        if let Some(remote) = self.repository().remote() {
            return remote
                .read(
                    self,
                    crate::remote::Read::ChangedFiles {
                        scope: scope.clone(),
                    },
                )
                .await;
        }

        let mut arguments = vec!["diff".into()];
        arguments.extend(scope.args());
        arguments.extend(["-T".into(), FILE_TEMPLATE.into()]);
        let mut files: Vec<ChangedFile> = parse_records(&self.read(&arguments).await?)?;
        for file in &files {
            validate_path(&file.path)?;
            if let Some(source) = &file.renamed_from {
                validate_path(source)?;
            }
        }

        if !files.is_empty() {
            let target = match scope {
                DiffScope::Change(revision) => revision,
                DiffScope::Comparison { target, .. } => target,
            };
            let conflicts = self.conflicted_paths(target).await?;
            for file in &mut files {
                file.has_conflict = conflicts.contains(&file.path);
            }
        }

        Ok(files)
    }

    pub async fn diff(&self, scope: &DiffScope, path: Option<&str>) -> Result<Vec<u8>> {
        if let Some(remote) = self.repository().remote() {
            return remote
                .read(
                    self,
                    crate::remote::Read::Diff {
                        scope: scope.clone(),
                        path: path.map(str::to_owned),
                    },
                )
                .await;
        }

        let mut arguments = vec!["diff".into()];
        arguments.extend(scope.args());
        arguments.push("--git".into());
        if let Some(path) = path {
            arguments.push(literal_fileset(path)?);
        }

        self.read(&arguments).await
    }

    pub async fn file_content(&self, revision: &str, path: &str) -> Result<Vec<u8>> {
        if let Some(remote) = self.repository().remote() {
            return remote
                .read(
                    self,
                    crate::remote::Read::FileContent {
                        revision: revision.into(),
                        path: path.into(),
                    },
                )
                .await;
        }

        self.read(&[
            "file".into(),
            "show".into(),
            "-r".into(),
            revision.into(),
            "--".into(),
            literal_fileset(path)?,
        ])
        .await
    }

    pub async fn file_content_if_exists(
        &self,
        revision: &str,
        path: &str,
    ) -> Result<Option<Vec<u8>>> {
        if let Some(remote) = self.repository().remote() {
            return remote
                .read(
                    self,
                    crate::remote::Read::FileContentIfExists {
                        revision: revision.into(),
                        path: path.into(),
                    },
                )
                .await;
        }

        match self.file_content(revision, path).await {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) => {
                let paths = self
                    .read(&[
                        "file".into(),
                        "list".into(),
                        "-r".into(),
                        revision.into(),
                        "--".into(),
                        literal_fileset(path)?,
                    ])
                    .await?;
                if paths.is_empty() {
                    Ok(None)
                } else {
                    Err(error)
                }
            }
        }
    }

    pub async fn search_data(
        &self,
        scope: &DiffScope,
    ) -> Result<(Vec<crate::ChangedLine>, Vec<ChangedFile>)> {
        if let Some(remote) = self.repository().remote() {
            return remote
                .read(
                    self,
                    crate::remote::Read::SearchData {
                        scope: scope.clone(),
                    },
                )
                .await;
        }

        let files = self.changed_files(scope).await?;
        let parse = |diff: &[u8], file: Option<&ChangedFile>| {
            let parsing = self.repository().diagnostics.start(
                "parse.searchDiff",
                self.repository().performance_context(),
                serde_json::json!({"bytes": diff.len()}),
            );
            let text = String::from_utf8_lossy(diff);
            let result = match file {
                Some(file) => crate::search::parse_file_diff(&text, file),
                None => crate::search::parse_git_diff(&text),
            };
            parsing.finish(
                if result.is_ok() { "completed" } else { "error" },
                serde_json::Value::Null,
            );
            result
        };
        let has_control_paths = files.iter().any(|file| {
            file.path.chars().any(char::is_control)
                || file
                    .renamed_from
                    .as_ref()
                    .is_some_and(|path| path.chars().any(char::is_control))
        });
        let lines = if has_control_paths {
            let mut lines: Vec<crate::ChangedLine> = Vec::new();
            for file in &files {
                let diff = self.diff(scope, Some(&file.path)).await?;
                let mut file_lines = parse(&diff, Some(file))?;
                let offset = lines.last().map_or(0, |line| line.edit_block + 1);
                for line in &mut file_lines {
                    line.edit_block += offset;
                }

                lines.extend(file_lines);
            }

            lines
        } else {
            parse(&self.diff(scope, None).await?, None)?
        };
        Ok((lines, files))
    }
}

pub(crate) fn is_metadata_path(path: &str) -> bool {
    path.split(['/', '\\'])
        .any(|component| matches!(component, ".kk" | ".jj" | ".git"))
}

pub(crate) fn validate_path(path: &str) -> Result<()> {
    ensure!(
        !path.is_empty() && !path.contains('\0'),
        "KK returned an invalid empty or NUL-containing path"
    );
    ensure!(
        Path::new(path)
            .components()
            .all(|component| matches!(component, Component::Normal(_))),
        "Path escapes the repository: {path}"
    );

    Ok(())
}

pub(crate) fn literal_fileset(path: &str) -> Result<String> {
    validate_path(path)?;

    Ok(format!("root-file:{}", serde_json::to_string(path)?))
}

pub(crate) fn parse_records<T: DeserializeOwned>(output: &[u8]) -> Result<Vec<T>> {
    std::str::from_utf8(output)?
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            serde_json::from_str(line.trim_start_matches('\u{1e}'))
                .context("Invalid structured KK output")
        })
        .collect()
}

async fn run_output(
    executable: &Path,
    directory: &Path,
    arguments: &[&str],
    diagnostics: &Diagnostics,
    parent: Option<&PerformanceContext>,
) -> Result<Output> {
    let command_id = uuid::Uuid::new_v4().to_string();
    let argv = ["--color", "never", "--no-pager"]
        .into_iter()
        .chain(arguments.iter().copied())
        .collect::<Vec<_>>();
    let environment = [
        "PATH",
        "KK_CONFIG",
        "JJ_CONFIG",
        "HOME",
        "XDG_CONFIG_HOME",
        "COLUMNS",
    ]
    .into_iter()
    .filter_map(|key| {
        std::env::var_os(key).map(|value| (key, value.to_string_lossy().into_owned()))
    })
    .collect::<std::collections::BTreeMap<_, _>>();
    let metadata = serde_json::json!({"commandId": command_id, "executable": executable, "cwd": directory, "argv": argv, "environment": environment, "environmentOverrides": {"COLUMNS": if std::env::var_os("COLUMNS").is_none() { Some("80") } else { None }}});
    diagnostics.log(format!("[COMMAND] {metadata}"));
    let span = diagnostics.start("process.kk", parent, metadata);
    let started = Instant::now();
    let mut command = Command::new(executable);
    command
        .current_dir(directory)
        .args(["--color", "never", "--no-pager"])
        .args(arguments)
        .stdin(Stdio::null())
        .kill_on_drop(true);
    if std::env::var_os("COLUMNS").is_none() {
        command.env("COLUMNS", "80");
    }

    let output = command.output().await.with_context(|| {
        format!(
            "Failed to launch {} in {}",
            executable.display(),
            directory.display()
        )
    });
    let output = match output {
        Ok(output) => output,
        Err(error) => {
            diagnostics.error(format!("[COMMAND {command_id}] {error:#}"));
            span.finish("error", serde_json::json!({"error": error.to_string()}));
            return Err(error);
        }
    };
    let metadata = serde_json::json!({
        "commandId": command_id,
        "exitCode": output.status.code(), "elapsedMs": started.elapsed().as_secs_f64() * 1000.0,
        "stdoutBytes": output.stdout.len(), "stderrBytes": output.stderr.len(),
    });
    diagnostics.log(format!("[COMMAND END] {metadata}"));
    if !output.status.success() {
        let error = anyhow::anyhow!(
            "KK {} failed ({}): {}",
            arguments.first().copied().unwrap_or("command"),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
        diagnostics.error(format!("[COMMAND {command_id}] {error:#}"));
        span.finish("error", metadata);
        return Err(error);
    }

    span.finish("completed", metadata);

    Ok(output)
}

const LOG_TEMPLATE: &str = r##""\x1e{\"changeId\":\"" ++ change_id.shortest(8) ++ "\",\"changeIdPrefix\":\"" ++ change_id.shortest(8).prefix() ++ "\",\"fullChangeId\":\"" ++ change_id ++ "\",\"description\":" ++ description.first_line().escape_json() ++ ",\"fullDescription\":" ++ description.escape_json() ++ ",\"commitId\":\"" ++ commit_id.shortest(8) ++ "\",\"fullCommitId\":\"" ++ commit_id ++ "\",\"authorName\":" ++ author.name().escape_json() ++ ",\"authorEmail\":" ++ stringify(author.email()).escape_json() ++ ",\"committerName\":" ++ committer.name().escape_json() ++ ",\"committerEmail\":" ++ stringify(committer.email()).escape_json() ++ ",\"commitTimestamp\":" ++ committer.timestamp().format("%s") ++ ",\"bookmarks\":" ++ json(local_bookmarks.map(|x| x.name())) ++ ",\"remoteBookmarkNames\":" ++ json(remote_bookmarks.map(|x| x.name())) ++ ",\"remoteBookmarkRemotes\":" ++ json(remote_bookmarks.map(|x| x.remote())) ++ ",\"tags\":" ++ json(tags.map(|x| x.name())) ++ ",\"parentChangeIds\":" ++ json(parents.map(|parent| stringify(parent.change_id().shortest(8)))) ++ ",\"parentCommitIds\":" ++ json(parents.map(|parent| stringify(parent.commit_id()))) ++ ",\"hasMultipleChildren\":" ++ if(self.contained_in("forks()"), "true", "false") ++ ",\"hasConflict\":" ++ if(conflict, "true", "false") ++ ",\"isWorkingCopy\":" ++ if(current_working_copy, "true", "false") ++ ",\"isRoot\":" ++ if(parents.len() == 0, "true", "false") ++ "}\n""##;
