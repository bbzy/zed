use crate::{
    ClassificationRequest, Diagnostics, DiffScope, FileConflict, FileMoveDirection, HistoryMode,
    LfsPointer, OperationReceipt, PreparedClassification, PreparedConflictResolution,
    PreparedFileMutation, Publication, Repository, RepositoryLifetime, Revision, SavedFile,
    SavedSnapshot, Snapshot, SparseCheckout, UncertainPublication,
};
use anyhow::{Context as _, Result, ensure};
use bincode::Options as _;
use futures::{
    FutureExt as _,
    channel::oneshot,
    future::{BoxFuture, Shared},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
};

pub use crate::remote_client::{Client, Transport};

pub type ResourceId = u128;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct DiagnosticCursor {
    stream: ResourceId,
    position: usize,
}

impl DiagnosticCursor {
    pub fn same_stream(self, other: Self) -> bool {
        self.stream == other.stream
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DiagnosticOutput {
    pub cursor: DiagnosticCursor,
    pub text: String,
}

const PROTOCOL_VERSION: u32 = 4;

pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    Ok(bincode::DefaultOptions::new().serialize(&(PROTOCOL_VERSION, value))?)
}

pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    let mut cursor = std::io::Cursor::new(bytes);
    let version: u32 = bincode::DefaultOptions::new()
        .with_limit(bytes.len() as u64)
        .deserialize_from(&mut cursor)?;
    ensure!(
        version == PROTOCOL_VERSION,
        "Unsupported KK Workbench protocol version {version} (expected {PROTOCOL_VERSION}). Update the remote Zed server to match this client"
    );
    let payload = bytes
        .get(cursor.position() as usize..)
        .context("Missing KK Workbench protocol payload")?;
    Ok(bincode::DefaultOptions::new()
        .with_limit(payload.len() as u64)
        .reject_trailing_bytes()
        .deserialize(payload)?)
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RepositoryInfo {
    pub id: ResourceId,
    pub root: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SnapshotInfo {
    pub repository: ResourceId,
    pub operation: String,
    pub performance_context: Option<crate::PerformanceContext>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Receipt {
    pub repository: ResourceId,
    pub data: Vec<u8>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Published {
    pub snapshot: SnapshotInfo,
    pub receipt: Receipt,
    pub synchronization_error: Option<String>,
    pub destination: Option<Revision>,
    pub range_start: Option<Revision>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Failure {
    pub message: String,
    pub receipt: Option<Receipt>,
}

impl From<anyhow::Error> for Failure {
    fn from(error: anyhow::Error) -> Self {
        Self {
            message: format!("{error:#}"),
            receipt: None,
        }
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for Failure {}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum Request {
    BeginDiffOpening {
        repository: ResourceId,
        id: ResourceId,
    },
    EndDiffOpening {
        id: ResourceId,
    },
    Discover {
        directory: PathBuf,
        executable: PathBuf,
    },
    Open {
        directory: PathBuf,
        executable: PathBuf,
    },
    DetachRepository {
        repository: ResourceId,
    },
    ReleaseRepository {
        repository: ResourceId,
    },
    Snapshot {
        repository: ResourceId,
        capture: bool,
    },
    SnapshotBeforeWrite {
        repository: ResourceId,
    },
    Read {
        snapshot: SnapshotInfo,
        operation: Read,
    },
    WorkingFile {
        repository: ResourceId,
        path: String,
    },
    WorkingFileInformation {
        repository: ResourceId,
        path: String,
        draft: Option<Vec<u8>>,
    },
    RelativeFilePath {
        repository: ResourceId,
        path: PathBuf,
    },
    LfsContent {
        repository: ResourceId,
        pointer: LfsPointer,
    },
    ExcludedSparseFiles {
        repository: ResourceId,
        paths: Vec<String>,
    },
    ExpandSparseCheckout {
        id: ResourceId,
        repository: ResourceId,
        paths: Vec<String>,
    },
    CancelSparseCheckout {
        id: ResourceId,
    },
    CaptureSaved {
        id: ResourceId,
        repository: ResourceId,
        paths: Vec<String>,
    },
    RecordSaved {
        repository: ResourceId,
        path: String,
    },
    FinishSaved {
        capture: ResourceId,
    },
    ReleaseSaved {
        capture: ResourceId,
    },
    Prepare {
        id: ResourceId,
        snapshot: SnapshotInfo,
        mutation: Mutation,
    },
    Publish {
        prepared: ResourceId,
    },
    ReleasePrepared {
        prepared: ResourceId,
    },
    PublicationStatus {
        receipt: Receipt,
    },
    Synchronize {
        receipt: Receipt,
    },
    Diagnostics {
        cursor: Option<DiagnosticCursor>,
        wait: bool,
    },
    ConfigureDiagnostics {
        categories: Vec<String>,
    },
    ExportDraft {
        path: PathBuf,
        text: String,
    },
    UpdateStaleWorkspace {
        repository: ResourceId,
    },
    SnapshotSavedWorkingCopy {
        repository: ResourceId,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum Read {
    Revisions {
        revset: Option<String>,
        limit: usize,
    },
    History {
        mode: HistoryMode,
        limit: usize,
    },
    HistoryContaining {
        mode: HistoryMode,
        limit: usize,
        revision: Revision,
    },
    CommitEmptiness {
        commits: Vec<String>,
    },
    ComparisonCandidates,
    ExactRevision {
        revset: String,
    },
    ResolveRevision {
        revision: Revision,
    },
    SearchRevisions {
        query: String,
        scope: Option<String>,
        working_copy: bool,
        limit: usize,
    },
    ComparisonBoundary {
        target: String,
        range_start: Option<String>,
    },
    CategoryCommits {
        range_start: Revision,
    },
    ChangedFiles {
        scope: DiffScope,
    },
    Diff {
        scope: DiffScope,
        path: Option<String>,
    },
    SearchData {
        scope: DiffScope,
    },
    FileContent {
        revision: String,
        path: String,
    },
    FileContentIfExists {
        revision: String,
        path: String,
    },
    FileInformation {
        revision: String,
        path: String,
    },
    CurrentFilePath {
        revision: String,
        path: String,
    },
    Annotate {
        revision: String,
        path: String,
    },
    ConflictedPaths {
        revision: String,
    },
    FileConflict {
        revision: String,
        path: String,
    },
    RefreshConflictSources {
        conflict: FileConflict,
    },
    VerifyFileVersion {
        previous: String,
        current: String,
        file: crate::ChangedFile,
    },
    RebaseClassification {
        previous: SnapshotInfo,
        request: ClassificationRequest,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum Mutation {
    Classify {
        previous: SnapshotInfo,
        request: ClassificationRequest,
        saved: Vec<ResourceId>,
    },
    Move {
        source: Revision,
        direction: FileMoveDirection,
        file: crate::ChangedFile,
    },
    Revert {
        scope: DiffScope,
        file: crate::ChangedFile,
    },
    ResolveConflict {
        conflict: FileConflict,
        acknowledged_commit: String,
        result: Option<Vec<u8>>,
        executable: Option<bool>,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum Response {
    Done,
    Data(Vec<u8>),
    Repository(RepositoryInfo),
    Discovery {
        repositories: Vec<RepositoryInfo>,
        errors: Vec<(PathBuf, String)>,
    },
    Snapshot(SnapshotInfo),
    Saved {
        id: ResourceId,
        version: u64,
        files: HashMap<String, SavedFile>,
    },
    Prepared {
        id: ResourceId,
        snapshot: SnapshotInfo,
        destination: Option<Revision>,
        range_start: Option<Revision>,
        receipt: Receipt,
    },
    Published(Published),
    SparsePendingSavedInputs,
    SparseCancelled,
}

type Publishing = Shared<BoxFuture<'static, std::result::Result<Published, Failure>>>;
type Finishing = Shared<BoxFuture<'static, std::result::Result<Response, Failure>>>;

#[derive(Default)]
struct State {
    closed: bool,
    repositories: HashMap<ResourceId, Repository>,
    saved: HashMap<ResourceId, (ResourceId, SavedSnapshot)>,
    released_saved: HashSet<ResourceId>,
    finishing: HashMap<ResourceId, Finishing>,
    prepared: HashMap<ResourceId, PreparedEntry>,
    preparing: HashSet<ResourceId>,
    released_prepared: HashSet<ResourceId>,
    publishing: HashMap<ResourceId, Publishing>,
    sparse: HashMap<ResourceId, Arc<AtomicBool>>,
    cancelled_sparse: HashSet<ResourceId>,
    openings: HashMap<ResourceId, (ResourceId, crate::DiffOpening)>,
    ended_openings: HashSet<ResourceId>,
}

impl State {
    fn detach_repository(&mut self, repository: ResourceId) {
        if let Some(lifetime) = self
            .repositories
            .get(&repository)
            .and_then(Repository::lifetime)
        {
            lifetime.invalidate();
        }

        self.prepared
            .retain(|_, entry| entry.repository != repository);
        self.saved.retain(|_, (owner, _)| *owner != repository);
        self.openings.retain(|_, (owner, _)| *owner != repository);
    }
}

enum Prepared {
    Classification(PreparedClassification),
    File(PreparedFileMutation),
    Conflict(PreparedConflictResolution),
}

struct PreparedEntry {
    repository: ResourceId,
    operation: Prepared,
    receipt: Receipt,
}

struct Preparing {
    state: Arc<Mutex<State>>,
    id: ResourceId,
}

struct SparseRequest {
    state: Arc<Mutex<State>>,
    id: ResourceId,
    active: Arc<AtomicBool>,
}

impl Drop for SparseRequest {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Release);
        match self.state.lock() {
            Ok(mut state) => {
                state.sparse.remove(&self.id);
            }
            Err(error) => log::error!("Could not release KK sparse checkout: {error}"),
        }
    }
}

impl Drop for Preparing {
    fn drop(&mut self) {
        match self.state.lock() {
            Ok(mut state) => {
                state.preparing.remove(&self.id);
            }
            Err(error) => log::error!("Could not release KK preparation: {error}"),
        }
    }
}

impl Prepared {
    async fn receipt(&self, repository: ResourceId) -> Result<Receipt> {
        let receipt = match self {
            Self::Classification(prepared) => prepared.receipt().await?,
            Self::File(prepared) => prepared.receipt().await?,
            Self::Conflict(prepared) => prepared.receipt().await?,
        };
        Ok(Receipt {
            repository,
            data: receipt.encode()?,
        })
    }

    fn response(&self, id: ResourceId, receipt: Receipt) -> Response {
        Response::Prepared {
            id,
            snapshot: snapshot_info(receipt.repository, self.snapshot()),
            destination: match self {
                Self::Classification(prepared) => Some(prepared.destination().clone()),
                _ => None,
            },
            range_start: match self {
                Self::Classification(prepared) => Some(prepared.range_start().clone()),
                _ => None,
            },
            receipt,
        }
    }

    fn snapshot(&self) -> &Snapshot {
        match self {
            Self::Classification(prepared) => prepared.snapshot(),
            Self::File(prepared) => prepared.snapshot(),
            Self::Conflict(prepared) => prepared.snapshot(),
        }
    }

    async fn publish(self, repository: ResourceId) -> Result<Published> {
        match self {
            Self::Classification(prepared) => {
                let result = prepared.publish().await?;
                let mut result_wire = published(repository, result.publication)?;
                result_wire.destination = Some(result.destination);
                result_wire.range_start = Some(result.range_start);
                Ok(result_wire)
            }
            Self::File(prepared) => published(repository, prepared.publish().await?),
            Self::Conflict(prepared) => published(repository, prepared.publish().await?),
        }
    }
}

#[derive(Clone)]
pub struct Service {
    state: Arc<Mutex<State>>,
    diagnostics: Diagnostics,
    diagnostic_stream: ResourceId,
}

impl Default for Service {
    fn default() -> Self {
        Self {
            state: Arc::new(Mutex::new(State::default())),
            diagnostics: Diagnostics::default(),
            diagnostic_stream: uuid::Uuid::new_v4().as_u128(),
        }
    }
}

impl Service {
    pub fn record_saved_file(&self, absolute: &Path) -> Result<()> {
        let repositories = {
            let state = self.state()?;
            ensure_open(&state)?;
            state.repositories.values().cloned().collect::<Vec<_>>()
        };
        if repositories.is_empty() {
            return Ok(());
        }

        let parent = absolute
            .parent()
            .context("Saved file has no parent directory")?;
        let canonical = std::fs::canonicalize(parent)?
            .join(absolute.file_name().context("Saved file has no name")?);
        let mut recorded = HashSet::new();
        for repository in repositories {
            if repository.check_lifetime().is_ok()
                && let Ok(relative) = canonical.strip_prefix(repository.root())
                && recorded.insert(repository.root().to_owned())
            {
                crate::record_saved_file(
                    repository.root(),
                    relative.to_str().context("Saved file path is not UTF-8")?,
                )?;
            }
        }

        Ok(())
    }

    pub fn close(&self) -> Result<()> {
        let mut state = self.state()?;
        if state.closed {
            return Ok(());
        }

        state.closed = true;
        for repository in state.repositories.values() {
            if let Some(lifetime) = repository.lifetime() {
                lifetime.invalidate();
            }
        }

        state.prepared.clear();
        state.openings.clear();
        state.saved.clear();
        for active in state.sparse.values() {
            active.store(false, Ordering::Release);
        }

        drop(state);
        self.diagnostics.log("KK remote connection closed");
        Ok(())
    }

    pub fn execute(
        &self,
        request: Request,
    ) -> BoxFuture<'static, std::result::Result<Response, Failure>> {
        if let Request::CaptureSaved {
            id,
            repository,
            paths,
        } = request
        {
            return futures::future::ready(
                self.capture_saved(id, repository, paths)
                    .map_err(Failure::from),
            )
            .boxed();
        }

        let service = self.clone();
        async move {
            if let Request::Publish { prepared } = request {
                return service.publish(prepared).await.map(Response::Published);
            }

            if let Request::FinishSaved { capture } = request {
                return service.finish_saved(capture).await;
            }

            service
                .execute_inner(request)
                .boxed()
                .await
                .map_err(Failure::from)
        }
        .boxed()
    }

    fn capture_saved(
        &self,
        id: ResourceId,
        repository: ResourceId,
        paths: Vec<String>,
    ) -> Result<Response> {
        let repository_handle = self.repository(repository)?;
        repository_handle.check_lifetime()?;
        {
            let state = self.state()?;
            ensure_open(&state)?;
            ensure!(
                !state.released_saved.contains(&id),
                "Saved capture was released"
            );
            if let Some((owner, saved)) = state.saved.get(&id) {
                ensure!(
                    *owner == repository,
                    "Saved capture belongs to another repository"
                );
                return Ok(Response::Saved {
                    id,
                    version: saved.version().context("Missing saved file version")?,
                    files: saved.files()?.clone(),
                });
            }
        }

        let saved = SavedSnapshot::capture(&repository_handle, paths)?;
        let mut state = self.state()?;
        repository_handle.check_lifetime()?;
        ensure_open(&state)?;
        ensure!(
            !state.released_saved.contains(&id),
            "Saved capture was released"
        );
        let saved = if let Some((owner, captured)) = state.saved.get(&id) {
            ensure!(
                *owner == repository,
                "Saved capture belongs to another repository"
            );
            captured.clone()
        } else {
            saved
        };
        let response = Response::Saved {
            id,
            version: saved.version().context("Missing saved file version")?,
            files: saved.files()?.clone(),
        };
        state.saved.insert(id, (repository, saved));
        Ok(response)
    }

    async fn execute_inner(&self, request: Request) -> Result<Response> {
        ensure!(!self.state()?.closed, "KK Workbench connection was closed");
        match request {
            Request::BeginDiffOpening { repository, id } => {
                let repository_handle = self.repository(repository)?;
                let opening = repository_handle.begin_diff_opening().await?;
                let mut state = self.state()?;
                ensure_open(&state)?;
                repository_handle.check_lifetime()?;
                if !state.ended_openings.contains(&id) {
                    ensure!(
                        !state.openings.contains_key(&id),
                        "Duplicate diff-opening request"
                    );
                    state.openings.insert(id, (repository, opening));
                }

                Ok(Response::Done)
            }
            Request::EndDiffOpening { id } => {
                let mut state = self.state()?;
                state.ended_openings.insert(id);
                state.openings.remove(&id);
                Ok(Response::Done)
            }
            Request::Discover {
                directory,
                executable,
            } => {
                let found = crate::discover_with_diagnostics(
                    &directory,
                    &executable,
                    self.diagnostics.clone(),
                    None,
                )
                .await;
                let repositories = found
                    .repositories
                    .into_iter()
                    .map(|repository| self.register(repository))
                    .collect::<Result<_>>()?;
                Ok(Response::Discovery {
                    repositories,
                    errors: found
                        .errors
                        .into_iter()
                        .map(|error| (error.path, error.message))
                        .collect(),
                })
            }
            Request::Open {
                directory,
                executable,
            } => {
                let repository = Repository::open_with_diagnostics(
                    &directory,
                    &executable,
                    self.diagnostics.clone(),
                    None,
                )
                .await?;
                Ok(Response::Repository(self.register(repository)?))
            }
            Request::DetachRepository { repository }
            | Request::ReleaseRepository { repository } => {
                let mut state = self.state()?;
                state.detach_repository(repository);
                if matches!(request, Request::ReleaseRepository { .. }) {
                    state.repositories.remove(&repository);
                }

                Ok(Response::Done)
            }
            Request::Snapshot {
                repository,
                capture,
            } => {
                let snapshot = self.repository(repository)?.snapshot(capture).await?;
                Ok(Response::Snapshot(snapshot_info(repository, &snapshot)))
            }
            Request::SnapshotBeforeWrite { repository } => {
                let snapshot = self.repository(repository)?.snapshot_before_write().await?;
                Ok(Response::Snapshot(snapshot_info(repository, &snapshot)))
            }
            Request::SnapshotSavedWorkingCopy { repository } => {
                let snapshot = self
                    .repository(repository)?
                    .snapshot_saved_working_copy()
                    .await?;
                Ok(Response::Snapshot(snapshot_info(repository, &snapshot)))
            }
            Request::UpdateStaleWorkspace { repository } => {
                let snapshot = self
                    .repository(repository)?
                    .update_stale_workspace()
                    .await?;
                Ok(Response::Snapshot(snapshot_info(repository, &snapshot)))
            }
            Request::Read {
                snapshot,
                operation,
            } => self.read(snapshot, operation).boxed().await,
            Request::WorkingFile { repository, path } => data(
                self.repository(repository)?
                    .working_file_content(&path)
                    .await?,
            ),
            Request::WorkingFileInformation {
                repository,
                path,
                draft,
            } => data(
                self.repository(repository)?
                    .working_file_information(&path, draft.as_deref())
                    .await?,
            ),
            Request::RelativeFilePath { repository, path } => data(
                self.repository(repository)?
                    .relative_file_path(&path)
                    .await?,
            ),
            Request::LfsContent {
                repository,
                pointer,
            } => data(self.repository(repository)?.lfs_content(&pointer).await?),
            Request::ExcludedSparseFiles { repository, paths } => data(
                self.repository(repository)?
                    .excluded_sparse_files(&paths)
                    .await?,
            ),
            Request::ExpandSparseCheckout {
                id,
                repository,
                paths,
            } => {
                let repository_handle = self.repository(repository)?;
                let request = {
                    let mut state = self.state()?;
                    ensure_open(&state)?;
                    if state.cancelled_sparse.contains(&id) {
                        return Ok(Response::SparseCancelled);
                    }

                    ensure!(
                        !state.sparse.contains_key(&id),
                        "Sparse checkout is already running"
                    );
                    let active = Arc::new(AtomicBool::new(true));
                    state.sparse.insert(id, active.clone());
                    SparseRequest {
                        state: self.state.clone(),
                        id,
                        active,
                    }
                };
                let result = repository_handle
                    .expand_sparse_checkout(&paths, || {
                        request.active.load(Ordering::Acquire)
                            && repository_handle.check_lifetime().is_ok()
                    })
                    .await?;
                Ok(match result {
                    SparseCheckout::Ready(snapshot) => {
                        Response::Snapshot(snapshot_info(repository, &snapshot))
                    }
                    SparseCheckout::Cancelled => Response::SparseCancelled,
                    SparseCheckout::PendingSavedInputs => Response::SparsePendingSavedInputs,
                })
            }
            Request::CancelSparseCheckout { id } => {
                let mut state = self.state()?;
                state.cancelled_sparse.insert(id);
                if let Some(active) = state.sparse.get(&id) {
                    active.store(false, Ordering::Release);
                }

                Ok(Response::Done)
            }
            Request::CaptureSaved { .. } => {
                anyhow::bail!("Saved files must be captured when the request arrives")
            }
            Request::RecordSaved { repository, path } => {
                crate::record_saved_file(self.repository(repository)?.root(), &path)?;
                Ok(Response::Done)
            }
            Request::ReleaseSaved { capture } => {
                let mut state = self.state()?;
                state.released_saved.insert(capture);
                state.saved.remove(&capture);
                state.finishing.remove(&capture);
                Ok(Response::Done)
            }
            Request::Prepare {
                id,
                snapshot,
                mutation,
            } => self.prepare(id, snapshot, mutation).boxed().await,
            Request::ReleasePrepared { prepared } => {
                let mut state = self.state()?;
                state.released_prepared.insert(prepared);
                state.prepared.remove(&prepared);
                state.publishing.remove(&prepared);
                Ok(Response::Done)
            }
            Request::PublicationStatus { receipt } => data(self.receipt(&receipt)?.status().await?),
            Request::Synchronize { receipt } => {
                let snapshot = self.receipt(&receipt)?.synchronize().await?;
                Ok(Response::Snapshot(snapshot_info(
                    receipt.repository,
                    &snapshot,
                )))
            }
            Request::Diagnostics { cursor, wait } => {
                let updates = self.diagnostics.subscribe();
                let previous = cursor.filter(|cursor| cursor.stream == self.diagnostic_stream);
                loop {
                    ensure_open(&*self.state()?)?;
                    let (position, text) = self
                        .diagnostics
                        .read_since(previous.map_or(0, |cursor| cursor.position));
                    if !wait || previous.is_none() || !text.is_empty() {
                        return data(DiagnosticOutput {
                            cursor: DiagnosticCursor {
                                stream: self.diagnostic_stream,
                                position,
                            },
                            text,
                        });
                    }

                    updates.recv().await?;
                }
            }
            Request::ConfigureDiagnostics { categories } => {
                self.diagnostics.configure(&categories);
                Ok(Response::Done)
            }
            Request::ExportDraft { path, text } => {
                ensure!(
                    path.is_absolute(),
                    "Draft export requires an absolute host path"
                );
                smol::fs::write(&path, text).await.with_context(|| {
                    format!("Could not export conflict draft to {}", path.display())
                })?;
                Ok(Response::Done)
            }
            Request::Publish { .. } => anyhow::bail!("Publication must use its retained result"),
            Request::FinishSaved { .. } => {
                anyhow::bail!("Saved finalization must use its retained result")
            }
        }
    }

    async fn finish_saved(&self, id: ResourceId) -> std::result::Result<Response, Failure> {
        let (finishing, work) = {
            let mut state = self.state().map_err(Failure::from)?;
            ensure_open(&state).map_err(Failure::from)?;
            if let Some(finishing) = state.finishing.get(&id) {
                (finishing.clone(), None)
            } else {
                let (repository, saved) = state
                    .saved
                    .remove(&id)
                    .context("Saved capture was released")
                    .map_err(Failure::from)?;
                state.released_saved.insert(id);
                let (sender, receiver) = oneshot::channel();
                let finishing = receiver
                    .map(|result| {
                        result.unwrap_or_else(|_| {
                            Err(Failure {
                                message: "KK saved-file finalization worker was lost".into(),
                                receipt: None,
                            })
                        })
                    })
                    .boxed()
                    .shared();
                state.finishing.insert(id, finishing.clone());
                (finishing, Some((repository, saved, sender)))
            }
        };
        if let Some((repository, saved, sender)) = work {
            smol::spawn(async move {
                let result = saved
                    .finish()
                    .await
                    .map(|snapshot| {
                        snapshot.map_or(Response::Done, |snapshot| {
                            Response::Snapshot(snapshot_info(repository, &snapshot))
                        })
                    })
                    .map_err(Failure::from);
                if sender.send(result).is_err() {
                    log::debug!("KK saved-file finalization waiter was released");
                }
            })
            .detach();
        }

        finishing.await
    }

    async fn read(&self, info: SnapshotInfo, operation: Read) -> Result<Response> {
        let snapshot = self.snapshot(&info)?;
        match operation {
            Read::Revisions { revset, limit } => {
                data(snapshot.revisions(revset.as_deref(), limit).await?)
            }
            Read::History { mode, limit } => data(snapshot.history(mode, limit).await?),
            Read::HistoryContaining {
                mode,
                limit,
                revision,
            } => data(snapshot.history_containing(mode, limit, &revision).await?),
            Read::CommitEmptiness { commits } => data(snapshot.commit_emptiness(&commits).await?),
            Read::ComparisonCandidates => data(snapshot.comparison_candidates().await?),
            Read::ExactRevision { revset } => data(snapshot.exact_revision(&revset).await?),
            Read::ResolveRevision { revision } => data(snapshot.resolve_revision(&revision).await?),
            Read::SearchRevisions {
                query,
                scope,
                working_copy,
                limit,
            } => data(
                snapshot
                    .search_revisions(&query, scope.as_deref(), working_copy, limit)
                    .await?,
            ),
            Read::ComparisonBoundary {
                target,
                range_start,
            } => data(
                snapshot
                    .comparison_boundary(&target, range_start.as_deref())
                    .await?,
            ),
            Read::CategoryCommits { range_start } => {
                data(snapshot.category_commits(&range_start).await?)
            }
            Read::ChangedFiles { scope } => data(snapshot.changed_files(&scope).await?),
            Read::Diff { scope, path } => data(snapshot.diff(&scope, path.as_deref()).await?),
            Read::SearchData { scope } => data(snapshot.search_data(&scope).await?),
            Read::FileContent { revision, path } => {
                data(snapshot.file_content(&revision, &path).await?)
            }
            Read::FileContentIfExists { revision, path } => {
                data(snapshot.file_content_if_exists(&revision, &path).await?)
            }
            Read::FileInformation { revision, path } => {
                data(snapshot.file_information(&revision, &path).await?)
            }
            Read::CurrentFilePath { revision, path } => {
                data(snapshot.current_file_path(&revision, &path).await?)
            }
            Read::Annotate { revision, path } => data(snapshot.annotate(&revision, &path).await?),
            Read::ConflictedPaths { revision } => data(snapshot.conflicted_paths(&revision).await?),
            Read::FileConflict { revision, path } => {
                data(snapshot.file_conflict(&revision, &path).await?)
            }
            Read::RefreshConflictSources { mut conflict } => {
                snapshot.refresh_conflict_sources(&mut conflict).await?;
                data(conflict)
            }
            Read::VerifyFileVersion {
                previous,
                current,
                file,
            } => {
                snapshot
                    .verify_file_version(&previous, &current, &file)
                    .await?;
                data(())
            }
            Read::RebaseClassification { previous, request } => {
                ensure!(
                    previous.repository == info.repository,
                    "Snapshots belong to different repositories"
                );
                data(
                    snapshot
                        .rebase_classification(&self.snapshot(&previous)?, &request)
                        .await?,
                )
            }
        }
    }

    async fn prepare(
        &self,
        id: ResourceId,
        info: SnapshotInfo,
        mutation: Mutation,
    ) -> Result<Response> {
        let reservation = {
            let mut state = self.state()?;
            ensure_open(&state)?;
            ensure!(
                !state.released_prepared.contains(&id),
                "Prepared operation was released"
            );
            if let Some(entry) = state.prepared.get(&id) {
                ensure!(
                    entry.repository == info.repository,
                    "Prepared operation belongs to another repository"
                );
                return Ok(entry.operation.response(id, entry.receipt.clone()));
            }

            ensure!(
                !state.publishing.contains_key(&id),
                "Prepared operation has already been published"
            );
            ensure!(
                state.preparing.insert(id),
                "Operation is still being prepared"
            );
            Preparing {
                state: self.state.clone(),
                id,
            }
        };
        let snapshot = self.snapshot(&info)?;
        snapshot.repository().check_lifetime()?;
        let prepared = match mutation {
            Mutation::Classify {
                previous,
                request,
                saved,
            } => {
                ensure!(
                    previous.repository == info.repository,
                    "Snapshots belong to different repositories"
                );
                let saved = {
                    let state = self.state()?;
                    saved
                        .into_iter()
                        .map(|id| {
                            let (owner, saved) =
                                state.saved.get(&id).context("Saved capture was released")?;
                            let owner = state
                                .repositories
                                .get(owner)
                                .context("Saved repository was released")?;
                            owner.check_lifetime()?;
                            ensure!(
                                owner.root() == snapshot.repository().root(),
                                "Saved input belongs to another repository"
                            );
                            Ok(saved.clone())
                        })
                        .collect::<Result<Vec<_>>>()?
                };
                let Some(prepared) = snapshot
                    .prepare_classification_with_saved(&self.snapshot(&previous)?, &request, saved)
                    .await?
                else {
                    return Ok(Response::Done);
                };
                Prepared::Classification(prepared)
            }
            Mutation::Move {
                source,
                direction,
                file,
            } => Prepared::File(
                snapshot
                    .prepare_file_move(&source, direction, &file)
                    .await?,
            ),
            Mutation::Revert { scope, file } => {
                Prepared::File(snapshot.prepare_file_revert(&scope, &file).await?)
            }
            Mutation::ResolveConflict {
                conflict,
                acknowledged_commit,
                result,
                executable,
            } => Prepared::Conflict(
                snapshot
                    .prepare_conflict_resolution(
                        &conflict,
                        &acknowledged_commit,
                        result.as_deref(),
                        executable,
                    )
                    .await?,
            ),
        };
        let receipt = prepared.receipt(info.repository).await?;
        let mut state = self.state()?;
        ensure!(!state.closed, "KK Workbench connection was closed");
        snapshot.repository().check_lifetime()?;
        ensure!(
            !state.released_prepared.contains(&id),
            "Prepared operation was released"
        );
        let response = prepared.response(id, receipt.clone());
        state.prepared.insert(
            id,
            PreparedEntry {
                repository: info.repository,
                operation: prepared,
                receipt,
            },
        );
        drop(state);
        drop(reservation);
        Ok(response)
    }

    async fn publish(&self, id: ResourceId) -> std::result::Result<Published, Failure> {
        let (publishing, work) = {
            let mut state = self.state().map_err(Failure::from)?;
            ensure_open(&state).map_err(Failure::from)?;
            if let Some(publishing) = state.publishing.get(&id) {
                (publishing.clone(), None)
            } else {
                let entry = state
                    .prepared
                    .remove(&id)
                    .context("Prepared operation was released")
                    .map_err(Failure::from)?;
                let (sender, receiver) = oneshot::channel();
                let publishing = receiver
                    .map(move |result| {
                        result.unwrap_or_else(|_| {
                            Err(Failure {
                                message: "KK publication worker was lost".into(),
                                receipt: Some(entry.receipt),
                            })
                        })
                    })
                    .boxed()
                    .shared();
                state.publishing.insert(id, publishing.clone());
                (
                    publishing,
                    Some((entry.repository, entry.operation, sender)),
                )
            }
        };
        if let Some((repository, prepared, sender)) = work {
            // Losing the RPC waiter cannot interrupt publication after the public operation began.
            smol::spawn(async move {
                let result = prepared.publish(repository).await.map_err(|error| {
                    let receipt = error
                        .downcast_ref::<UncertainPublication>()
                        .map(|uncertain| {
                            uncertain
                                .receipt
                                .encode()
                                .map(|data| Receipt { repository, data })
                        })
                        .transpose();
                    match receipt {
                        Ok(receipt) => Failure {
                            message: format!("{error:#}"),
                            receipt,
                        },
                        Err(encoding) => Failure {
                            message: format!(
                                "{error:#}; cannot encode recovery receipt: {encoding:#}"
                            ),
                            receipt: None,
                        },
                    }
                });
                if sender.send(result).is_err() {
                    log::debug!("KK publication waiter was released");
                }
            })
            .detach();
        }

        publishing.await
    }

    fn register(&self, repository: Repository) -> Result<RepositoryInfo> {
        let mut state = self.state()?;
        ensure_open(&state)?;
        let id = uuid::Uuid::new_v4().as_u128();
        let info = RepositoryInfo {
            id,
            root: repository.root().to_owned(),
        };
        let lifetime = state
            .repositories
            .values()
            .find(|existing| existing.root() == repository.root())
            .and_then(Repository::lifetime)
            .map(RepositoryLifetime::sharing_openings)
            .unwrap_or_default();
        state
            .repositories
            .insert(id, repository.with_lifetime(lifetime));
        Ok(info)
    }

    fn repository(&self, id: ResourceId) -> Result<Repository> {
        self.state()?
            .repositories
            .get(&id)
            .cloned()
            .context("Remote KK repository was released")
    }

    fn snapshot(&self, info: &SnapshotInfo) -> Result<Snapshot> {
        let repository = self.repository(info.repository)?;
        let repository = if let Some(context) = &info.performance_context {
            let span = self.diagnostics.start(
                "remote.bindSnapshot",
                None,
                serde_json::json!({"upstreamContext": context, "repository": info.repository.to_string()}),
            );
            let context = span.context().cloned();
            span.finish(
                "completed",
                serde_json::json!({"boundary": "remote snapshot bound"}),
            );
            repository.with_performance_context(context)
        } else {
            repository
        };
        Snapshot::from_operation(repository, info.operation.clone())
    }

    fn receipt(&self, receipt: &Receipt) -> Result<OperationReceipt> {
        OperationReceipt::decode(self.repository(receipt.repository)?, &receipt.data)
    }

    fn state(&self) -> Result<MutexGuard<'_, State>> {
        self.state
            .lock()
            .map_err(|_| anyhow::anyhow!("KK backend service is unavailable"))
    }
}

fn ensure_open(state: &State) -> Result<()> {
    ensure!(!state.closed, "KK Workbench connection was closed");
    Ok(())
}

fn data(value: impl Serialize) -> Result<Response> {
    Ok(Response::Data(encode(&value)?))
}

fn snapshot_info(repository: ResourceId, snapshot: &Snapshot) -> SnapshotInfo {
    SnapshotInfo {
        repository,
        operation: snapshot.operation().to_owned(),
        performance_context: None,
    }
}

fn published(repository: ResourceId, publication: Publication) -> Result<Published> {
    Ok(Published {
        snapshot: snapshot_info(repository, &publication.snapshot),
        receipt: Receipt {
            repository,
            data: publication.receipt.encode()?,
        },
        synchronization_error: publication.synchronization_error,
        destination: None,
        range_start: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_handles_share_diff_priority_but_keep_independent_lifetimes() -> Result<()> {
        use futures::FutureExt as _;
        smol::block_on(async {
            let directory = tempfile::tempdir()?;
            let output = smol::process::Command::new("kk")
                .current_dir(directory.path())
                .args(["init", "."])
                .output()
                .await?;
            ensure!(
                output.status.success(),
                "Could not create test repository: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let repository = Repository::open(directory.path(), Path::new("kk")).await?;
            let service = Service::default();
            let first = service.register(repository.clone())?;
            let second = service.register(repository)?;
            service.execute(Request::EndDiffOpening { id: 100 }).await?;
            service
                .execute(Request::BeginDiffOpening {
                    repository: first.id,
                    id: 100,
                })
                .await?;
            let first = service.repository(first.id)?;
            let second = service.repository(second.id)?;
            second
                .yield_to_diff_opening()
                .now_or_never()
                .context("An ended opening must not block a later split")??;
            let opening = first.begin_diff_opening().await?;
            let mut wait = Box::pin(second.yield_to_diff_opening());
            assert!(
                wait.as_mut().now_or_never().is_none(),
                "new handles must yield to reads using an older handle"
            );
            first.lifetime().context("first lifetime")?.invalidate();
            wait.await?;
            second.check_lifetime()?;
            let second_opening = second.begin_diff_opening().await?;
            first.lifetime().context("first lifetime")?.invalidate();
            let mut wait = Box::pin(second.yield_to_diff_opening());
            assert!(
                wait.as_mut().now_or_never().is_none(),
                "closing another handle must retain this opening"
            );
            drop(second_opening);
            wait.await?;
            drop(opening);
            Ok(())
        })
    }

    #[test]
    fn kk_diff_history_protocol_checks_version_before_reading_revision_fields() {
        let old_header = bincode::DefaultOptions::new()
            .serialize(&2u32)
            .expect("old protocol header");
        let error =
            decode::<Vec<Revision>>(&old_header).expect_err("old revision layout is incompatible");
        assert!(
            error
                .to_string()
                .contains("protocol version 2 (expected 4)"),
            "{error:#}"
        );
        let revision: Revision = serde_json::from_value(serde_json::json!({
            "changeId":"change", "fullChangeId":"change", "commitId":"commit", "fullCommitId":"commit",
            "description":"subject", "fullDescription":"subject\nbody", "authorName":"Author", "authorEmail":"author@example.com",
            "committerName":"Committer", "committerEmail":"committer@example.com", "commitTimestamp":42,
            "bookmarks":[], "remoteBookmarkNames":[], "remoteBookmarkRemotes":[], "tags":[],
            "parentChangeIds":[], "parentCommitIds":[], "hasMultipleChildren":false, "hasConflict":false, "isWorkingCopy":false, "isRoot":false
        })).expect("revision");
        let mut bytes = encode(&vec![revision]).expect("encode current history");
        let decoded: Vec<Revision> = decode(&bytes).expect("decode current history");
        let revision = decoded.first().expect("decoded revision");
        assert_eq!(revision.commit_timestamp, Some(42));
        assert_eq!(revision.full_description, "subject\nbody");
        bytes.push(0);
        assert!(
            decode::<Vec<Revision>>(&bytes).is_err(),
            "trailing bytes are rejected"
        );
    }
}
