use crate::{
    Completion, CompletionResponse, CompletionSource, LocationLink, LspStore, Project, ProjectPath,
};
use crate::{
    CoreCompletion, CoreCompletionResponse,
    lsp_command::{GetCompletions, GetDeclarations, GetDefinitions, GetReferences, LspCommand},
};
use anyhow::{Context as _, Result};
use client::TypedEnvelope;
use collections::{HashMap, HashSet};
use futures::{
    FutureExt as _,
    future::{AbortHandle, Abortable, BoxFuture, Shared, WeakShared},
};
use gpui::{App, AppContext as _, Context, Entity, FutureExt as _, Task};
use language::{
    Buffer, BufferSnapshot, CodeLabel, Language, LanguageRegistry, Location, ToPointUtf16,
    language_settings::AllLanguageSettings,
    symbol_index::{Document, Index, SymbolLanguage, SymbolLocation},
};
use parking_lot::Mutex;
use rpc::{AnyProtoClient, proto};
use settings::SettingsStore;
use std::{
    collections::VecDeque,
    fmt,
    io::Read as _,
    path::PathBuf,
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use util::rel_path::RelPath;

#[derive(Clone)]
pub(super) struct SymbolQueryTrace(Arc<QueryTraceState>);

struct QueryTraceState {
    id: String,
    operation: &'static str,
    role: &'static str,
    stage: Mutex<&'static str>,
    finished: AtomicBool,
    started: Instant,
}

impl SymbolQueryTrace {
    pub(super) fn new(operation: &'static str) -> Self {
        Self::with_id(
            format!("{}-{}", std::process::id(), rand::random::<u64>()),
            operation,
            "client",
        )
    }

    fn with_id(id: String, operation: &'static str, role: &'static str) -> Self {
        Self(Arc::new(QueryTraceState {
            id,
            operation,
            role,
            stage: Mutex::new("created"),
            finished: AtomicBool::new(false),
            started: Instant::now(),
        }))
    }

    pub(super) fn begin(&self, buffer: &Entity<Buffer>, position: text::PointUtf16, cx: &App) {
        if log::log_enabled!(target: "project::symbol_index", self.0.level()) {
            let buffer = buffer.read(cx);
            self.record(
                "request",
                format_args!(
                    "file={:?} position={position:?} language={:?}",
                    buffer.file().map(|file| file
                        .file_system_abs_path(cx)
                        .unwrap_or_else(|| file.full_path(cx))),
                    buffer.language().map(|language| language.name()),
                ),
            );
        } else {
            *self.0.stage.lock() = "request";
        }
    }

    pub(super) fn record(&self, stage: &'static str, details: fmt::Arguments<'_>) {
        *self.0.stage.lock() = stage;
        log::log!(target: "project::symbol_index", self.0.level(), "syntax-symbols request={} role={} operation={} stage={} elapsed_ms={} {}", self.0.id, self.0.role, self.0.operation, stage, self.0.started.elapsed().as_millis(), details);
    }

    pub(super) fn finish(&self, stage: &'static str, details: fmt::Arguments<'_>) {
        self.record(stage, details);
        self.0.finished.store(true, Ordering::Relaxed);
    }
}

impl QueryTraceState {
    fn level(&self) -> log::Level {
        if self.operation == "completions" {
            log::Level::Debug
        } else {
            log::Level::Info
        }
    }
}

impl Drop for QueryTraceState {
    fn drop(&mut self) {
        if !self.finished.load(Ordering::Relaxed) && *self.stage.get_mut() != "created" {
            log::log!(target: "project::symbol_index", self.level(), "syntax-symbols request={} role={} operation={} stage=cancelled last_stage={} elapsed_ms={}", self.id, self.role, self.operation, *self.stage.lock(), self.started.elapsed().as_millis());
        }
    }
}

const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
const CACHE_BYTES: usize = 16 * 1024 * 1024;
const DISK_INDEX_BYTES: usize = 64 * 1024 * 1024;
const OVERLAY_INDEX_BYTES: usize = 16 * 1024 * 1024;
const REMOTE_QUERY_TIMEOUT: Duration = Duration::from_secs(10);

type IndexTask = BoxFuture<'static, Result<Arc<ProjectIndex>, Arc<anyhow::Error>>>;
type SharedIndex = Shared<IndexTask>;

#[derive(Default)]
pub(super) struct SymbolIndexStore {
    cache: Arc<Mutex<DocumentCache>>,
    pub(super) jobs: Arc<Mutex<IndexJobs>>,
    build_lock: Arc<futures::lock::Mutex<()>>,
    remote_queries: Arc<Mutex<RemoteQueries>>,
}

#[derive(Default)]
pub(super) struct IndexJobs {
    pub(super) generation: usize,
    pub(super) settings_generation: usize,
    disk: Option<Arc<DiskIndex>>,
    pending: HashMap<String, IndexJob>,
}

struct IndexJob {
    task: WeakShared<IndexTask>,
    ready: Weak<ProjectIndex>,
    trace_id: String,
}

struct DocumentCache {
    budget: usize,
    entries: HashMap<ProjectPath, CachedDocument>,
    bytes: usize,
    evictions: usize,
}

struct CachedDocument {
    stamp: String,
    document: Arc<Document>,
    accessed_at: Instant,
}

impl Default for DocumentCache {
    fn default() -> Self {
        Self {
            budget: CACHE_BYTES,
            entries: HashMap::default(),
            bytes: 0,
            evictions: 0,
        }
    }
}

impl DocumentCache {
    fn get(&mut self, path: &ProjectPath, stamp: &str) -> Option<Arc<Document>> {
        if self
            .entries
            .get(path)
            .is_some_and(|entry| entry.stamp != stamp)
        {
            self.remove(path);
        }
        let entry = self.entries.get_mut(path)?;
        entry.accessed_at = Instant::now();
        Some(entry.document.clone())
    }

    fn remove(&mut self, path: &ProjectPath) {
        if let Some(entry) = self.entries.remove(path) {
            self.bytes -= entry.document.memory_cost();
        }
    }

    fn insert(&mut self, path: ProjectPath, stamp: String, document: Arc<Document>) {
        self.remove(&path);
        let cost = document.memory_cost();
        if cost > self.budget {
            return;
        }
        while self.bytes + cost > self.budget {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.accessed_at)
                .map(|(path, _)| path.clone())
            else {
                return;
            };
            self.remove(&oldest);
            self.evictions += 1;
        }
        self.bytes += cost;
        self.entries.insert(
            path,
            CachedDocument {
                stamp,
                document,
                accessed_at: Instant::now(),
            },
        );
    }
}

#[derive(Default)]
struct RemoteQueries {
    active: HashMap<String, AbortHandle>,
    cancelled: VecDeque<String>,
}

struct RemoteQueryRegistration {
    queries: Arc<Mutex<RemoteQueries>>,
    trace_id: String,
}

impl Drop for RemoteQueryRegistration {
    fn drop(&mut self) {
        self.queries.lock().active.remove(&self.trace_id);
    }
}

struct RemoteQueryCancellation {
    client: AnyProtoClient,
    request: Option<proto::CancelSyntaxSymbols>,
}

impl RemoteQueryCancellation {
    fn disarm(&mut self) {
        self.request = None;
    }
}

impl Drop for RemoteQueryCancellation {
    fn drop(&mut self) {
        if let Some(request) = self.request.take() {
            if let Err(error) = self.client.send(request) {
                log::warn!("Cancelling remote syntax query: {error:#}");
            }
        }
    }
}

struct IndexedFile {
    path: Option<ProjectPath>,
    document: Arc<Document>,
}

struct ProjectIndex {
    files: Vec<IndexedFile>,
    source: usize,
    source_snapshot: BufferSnapshot,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DiskGeneration {
    worktree: usize,
    settings: usize,
    languages: usize,
}

struct DiskIndex {
    generation: DiskGeneration,
    entries: HashMap<ProjectPath, DiskIndexEntry>,
    limit_reached: bool,
}

#[derive(Clone)]
struct DiskIndexEntry {
    modified: Option<fs::MTime>,
    size: u64,
    inode: u64,
    document: Option<Arc<Document>>,
}

impl DiskIndexEntry {
    fn matches(&self, file: &worktree::Entry) -> bool {
        self.modified.is_some()
            && self.modified == file.mtime
            && self.size == file.size
            && self.inode == file.inode
    }
}

struct DiskLanguageSettings {
    global: AllLanguageSettings,
    local: Vec<(worktree::WorktreeId, Arc<RelPath>, AllLanguageSettings)>,
    modeline_lines: usize,
}

impl DiskLanguageSettings {
    fn new(cx: &App) -> Self {
        let settings = cx.global::<SettingsStore>();
        Self {
            global: settings.get::<AllLanguageSettings>(None).clone(),
            local: settings
                .get_all_locals::<AllLanguageSettings>()
                .into_iter()
                .map(|(worktree, path, settings)| (worktree, path, settings.clone()))
                .collect(),
            modeline_lines: settings
                .raw_user_settings()
                .and_then(|settings| settings.content.modeline_lines)
                .or(settings.raw_default_settings().modeline_lines)
                .unwrap_or(5),
        }
    }

    fn language(
        &self,
        languages: &Arc<LanguageRegistry>,
        path: &ProjectPath,
        full_path: &std::path::Path,
        absolute_path: &std::path::Path,
        path_style: util::paths::PathStyle,
        content: &text::Rope,
    ) -> Option<language::LanguageId> {
        if let Some(mode) =
            language::modeline::parse_modeline_from_rope(content, self.modeline_lines)
                .and_then(|settings| settings.mode)
        {
            return languages
                .available_language_for_modeline_name(&mode)
                .map(|language| language.id());
        }

        let settings = self
            .local
            .iter()
            .rev()
            .find_map(|(worktree, prefix, settings)| {
                (*worktree == path.worktree_id && path.path.starts_with(prefix)).then_some(settings)
            })
            .unwrap_or(&self.global);
        languages.language_for_file_with_settings(
            path.path
                .file_name()
                .or_else(|| absolute_path.file_name().and_then(|name| name.to_str())),
            &[full_path, absolute_path],
            Some(content),
            path_style,
            settings,
        )
    }
}

impl Project {
    pub fn completions_for_syntax_trigger(
        &self,
        buffer: &Entity<Buffer>,
        position: language::Anchor,
        text: &str,
        cx: &mut Context<Self>,
    ) -> Option<Task<Result<Vec<CompletionResponse>>>> {
        self.is_syntax_completion_trigger(buffer, position, text, cx)
            .then(|| self.completions_with_provider(buffer, position, None, cx))
    }

    fn is_syntax_completion_trigger(
        &self,
        buffer: &Entity<Buffer>,
        position: language::Anchor,
        text: &str,
        cx: &App,
    ) -> bool {
        if !self.supports_syntax_symbols(buffer, cx) {
            return false;
        }
        if text == "." {
            return true;
        }
        let snapshot = buffer.read(cx).snapshot();
        let mut previous = snapshot.reversed_chars_at(position);
        matches!(
            (text, previous.next(), previous.next()),
            (">", Some('>'), Some('-')) | (":", Some(':'), Some(':'))
        )
    }

    pub(crate) fn supports_syntax_symbols(&self, buffer: &Entity<Buffer>, cx: &App) -> bool {
        let store = self.lsp_store.read(cx);
        (self.is_local()
            || (self.is_via_remote_server()
                && store
                    .upstream_client()
                    .is_some_and(|(client, _)| client.supports_syntax_symbols())))
            && store.syntax_language_supported(buffer, cx)
    }
}

impl LspStore {
    fn request_remote_syntax_symbols(
        &self,
        buffer: &Entity<Buffer>,
        offset: usize,
        kind: proto::get_syntax_symbols::Kind,
        trace: SymbolQueryTrace,
        cx: &mut Context<Self>,
    ) -> Task<Result<proto::GetSyntaxSymbolsResponse>> {
        let Some((client, project_id)) = self.upstream_client() else {
            return Task::ready(Err(anyhow::anyhow!(
                "Remote symbol index connection unavailable"
            )));
        };
        let source = buffer.read(cx);
        let request = proto::GetSyntaxSymbols {
            project_id,
            buffer_id: source.remote_id().into(),
            position: Some(language::proto::serialize_anchor(
                &source.anchor_before(offset),
            )),
            version: language::proto::serialize_version(&source.version()),
            kind: kind.into(),
            trace_id: trace.0.id.clone(),
        };
        send_remote_symbol_query(client, request, trace, cx)
    }

    pub(super) async fn handle_get_syntax_symbols(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::GetSyntaxSymbols>,
        mut cx: gpui::AsyncApp,
    ) -> Result<proto::GetSyntaxSymbolsResponse> {
        use proto::get_syntax_symbols::Kind;
        let peer_id = envelope.original_sender_id().unwrap_or_default();
        let request = envelope.payload;
        anyhow::ensure!(
            !request.trace_id.is_empty()
                && request.trace_id.len() <= 64
                && request
                    .trace_id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'),
            "Invalid symbol query trace id"
        );
        let kind = Kind::try_from(request.kind).context("Invalid symbol query kind")?;
        let operation = match kind {
            Kind::Definition => "definition",
            Kind::Declaration => "declaration",
            Kind::References => "references",
            Kind::Completions => "completions",
        };
        let trace = SymbolQueryTrace::with_id(request.trace_id.clone(), operation, "host");
        trace.record(
            "remote_request",
            format_args!("buffer_id={}", request.buffer_id),
        );
        let (abort, registration) = AbortHandle::new_pair();
        let query_registration = this.update(
            &mut cx,
            |store, _| -> Result<Option<RemoteQueryRegistration>> {
                let queries = store.symbol_index.remote_queries.clone();
                let mut pending = queries.lock();
                if let Some(position) = pending
                    .cancelled
                    .iter()
                    .position(|id| id == &request.trace_id)
                {
                    pending.cancelled.remove(position);
                    return Ok(None);
                }
                anyhow::ensure!(
                    !pending.active.contains_key(&request.trace_id),
                    "Duplicate syntax query id"
                );
                pending.active.insert(request.trace_id.clone(), abort);
                drop(pending);
                Ok(Some(RemoteQueryRegistration {
                    queries,
                    trace_id: request.trace_id.clone(),
                }))
            },
        )?;
        let Some(query_registration) = query_registration else {
            trace.finish("cancelled", format_args!("reason=cancelled_before_start"));
            return Err(anyhow::anyhow!("Syntax query cancelled before start"));
        };
        let result = Abortable::new(
            async {
                let buffer_id = text::BufferId::new(request.buffer_id)?;
                let buffer = this.read_with(&cx, |store, cx| {
                    store.buffer_store().read(cx).get_existing(buffer_id)
                })?;
                let position = request
                    .position
                    .and_then(language::proto::deserialize_anchor)
                    .context("Invalid symbol query position")?;
                let version = language::proto::deserialize_version(&request.version);
                trace.record(
                    "buffer_sync",
                    format_args!(
                        "file={:?}",
                        buffer.read_with(&cx, |buffer, cx| buffer.file().map(|file| file
                            .file_system_abs_path(cx)
                            .unwrap_or_else(|| file.full_path(cx))))
                    ),
                );
                let executor = cx.background_executor().clone();
                buffer
                    .update(&mut cx, |buffer, _| buffer.wait_for_version(version))
                    .with_timeout(REMOTE_QUERY_TIMEOUT, &executor)
                    .await
                    .context("Timed out waiting for the syntax query buffer version")??;
                let offset = buffer
                    .read_with(&cx, |buffer, _| {
                        buffer.snapshot().try_offset_for_anchor(&position)
                    })
                    .context("Symbol query position does not resolve in the source buffer")?;
                let mut response = proto::GetSyntaxSymbolsResponse::default();
                match kind {
                    Kind::Definition | Kind::Declaration => {
                        let locations = this
                            .update(&mut cx, |store, cx| {
                                store.syntax_definitions(
                                    &buffer,
                                    offset,
                                    kind == Kind::Declaration,
                                    trace.clone(),
                                    cx,
                                )
                            })
                            .await?
                            .unwrap_or_default();
                        this.update(&mut cx, |store, cx| {
                            let version = buffer.read(cx).version();
                            if kind == Kind::Definition {
                                response.definitions = Some(GetDefinitions::response_to_proto(
                                    locations, store, peer_id, &version, cx,
                                ));
                            } else {
                                response.declarations = Some(GetDeclarations::response_to_proto(
                                    locations, store, peer_id, &version, cx,
                                ));
                            }
                        });
                    }
                    Kind::References => {
                        let locations = this
                            .update(&mut cx, |store, cx| {
                                store.syntax_references(&buffer, offset, trace.clone(), cx)
                            })
                            .await?
                            .unwrap_or_default();
                        response.references = Some(this.update(&mut cx, |store, cx| {
                            GetReferences::response_to_proto(
                                locations,
                                store,
                                peer_id,
                                &buffer.read(cx).version(),
                                cx,
                            )
                        }));
                    }
                    Kind::Completions => {
                        let responses = this
                            .update(&mut cx, |store, cx| {
                                store.syntax_completions(&buffer, offset, trace.clone(), cx)
                            })
                            .await?;
                        let mut completions = Vec::new();
                        for completion in responses
                            .into_iter()
                            .flat_map(|response| response.completions)
                        {
                            response.completion_labels.push(completion.label.text);
                            completions.push(CoreCompletion {
                                replace_range: completion.replace_range,
                                new_text: completion.new_text,
                                source: completion.source,
                            });
                        }
                        let core = CoreCompletionResponse {
                            completions,
                            is_incomplete: true,
                        };
                        response.completions = Some(this.update(&mut cx, |store, cx| {
                            GetCompletions::response_to_proto(
                                core,
                                store,
                                peer_id,
                                &buffer.read(cx).version(),
                                cx,
                            )
                        }));
                    }
                }
                anyhow::Ok(response)
            },
            registration,
        )
        .await;
        drop(query_registration);
        let result = match result {
            Ok(result) => result,
            Err(_) => {
                trace.finish("cancelled", format_args!("reason=client_cancelled"));
                return Err(anyhow::anyhow!("Syntax query cancelled"));
            }
        };
        match &result {
            Ok(_) => trace.finish("complete", format_args!("response_ready=true")),
            Err(error) => trace.finish("failed", format_args!("error={error:#}")),
        }
        result
    }

    pub(super) async fn handle_cancel_syntax_symbols(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::CancelSyntaxSymbols>,
        mut cx: gpui::AsyncApp,
    ) -> Result<()> {
        let id = envelope.payload.trace_id;
        anyhow::ensure!(
            !id.is_empty()
                && id.len() <= 64
                && id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'),
            "Invalid syntax cancellation id"
        );
        let abort = this.update(&mut cx, |store, _| {
            let mut queries = store.symbol_index.remote_queries.lock();
            if let Some(abort) = queries.active.get(&id) {
                return Some(abort.clone());
            }
            // Cancellation can overtake the queued background request.
            if !queries.cancelled.contains(&id) {
                if queries.cancelled.len() == 512 {
                    queries.cancelled.pop_front();
                }
                queries.cancelled.push_back(id);
            }
            None
        });
        if let Some(abort) = abort {
            abort.abort();
        }
        Ok(())
    }

    pub(super) fn syntax_language_supported(&self, buffer: &Entity<Buffer>, cx: &App) -> bool {
        buffer.read(cx).language().is_some_and(|language| {
            SymbolLanguage::for_name(language.name().as_ref()).is_some()
                && language.grammar().is_some()
        })
    }

    fn supports_syntax_symbols(&self, buffer: &Entity<Buffer>, cx: &App) -> bool {
        self.as_local().is_some() && self.syntax_language_supported(buffer, cx)
    }

    fn prepare_symbol_query(
        &self,
        buffer: &Entity<Buffer>,
        offset: usize,
        completion: bool,
        trace: SymbolQueryTrace,
        cx: &mut Context<Self>,
    ) -> Task<Result<Option<Arc<ProjectIndex>>>> {
        let snapshot = buffer.read(cx).snapshot();
        if snapshot.len() as u64 > MAX_FILE_BYTES {
            return Task::ready(Ok(None));
        }

        let path = project_path(buffer, cx);
        let cache = self.symbol_index.cache.clone();
        let validation = cx.background_spawn({
            let snapshot = snapshot.clone();
            async move {
                let language = snapshot
                    .language()
                    .context("No language for syntax index")?;
                let document = cached_buffer_document(&cache, path.as_ref(), &snapshot, language)?;
                anyhow::Ok(
                    document.memory_cost() <= OVERLAY_INDEX_BYTES
                        && if completion {
                            document.completion_context(offset).is_some()
                        } else {
                            document.occurrence_at(offset).is_some()
                        },
                )
            }
        });
        let buffer = buffer.clone();
        cx.spawn(async move |store, cx| {
            if !validation.await? {
                trace.record(
                    "skipped",
                    format_args!("reason=no_identifier_at_position offset={offset}"),
                );
                return Ok(None);
            }
            let task = store.update(cx, |store, cx| {
                if buffer.read(cx).version() != *snapshot.version() {
                    return Task::ready(Ok(None));
                }
                let task = store.prepare_symbol_index(&buffer, Some(trace.clone()), cx);
                cx.background_spawn(async move { task.await.map(Some) })
            })?;
            task.await
        })
    }

    fn prepare_symbol_index(
        &self,
        source: &Entity<Buffer>,
        trace: Option<SymbolQueryTrace>,
        cx: &mut Context<Self>,
    ) -> Task<Result<Arc<ProjectIndex>>> {
        let source_snapshot = source.read(cx).snapshot();
        let source_path = project_path(source, cx);
        let overlays: HashMap<_, _> = self
            .buffer_store()
            .read(cx)
            .buffers()
            .filter_map(|buffer| Some((project_path(&buffer, cx)?, buffer.read(cx).snapshot())))
            .collect();
        let worktrees: Vec<_> = self
            .worktree_store()
            .read(cx)
            .worktrees()
            .filter(|worktree| worktree.read(cx).is_local())
            .map(|worktree| {
                let worktree = worktree.read(cx);
                (worktree.snapshot(), worktree.full_path(RelPath::empty()))
            })
            .collect();
        let languages = self.languages.clone();
        let Some(fs) = self.symbol_index_fs() else {
            return Task::ready(Err(anyhow::anyhow!(
                "Syntax indexing requires the project host filesystem"
            )));
        };
        let cache = self.symbol_index.cache.clone();
        let jobs = self.symbol_index.jobs.clone();
        let mut overlay_keys: Vec<_> = overlays
            .iter()
            .map(|(path, snapshot)| {
                format!(
                    "{path:?}:{:?}:{:?}:{:?}",
                    snapshot.remote_id(),
                    snapshot.version(),
                    snapshot.language().map(|language| language.id())
                )
            })
            .collect();
        overlay_keys.sort();
        let mut state = jobs.lock();
        let generation = DiskGeneration {
            worktree: state.generation,
            settings: state.settings_generation,
            languages: languages.version(),
        };
        let key = format!(
            "{generation:?}:{source_path:?}:{:?}:{:?}:{:?}:{overlay_keys:?}",
            source_snapshot.remote_id(),
            source_snapshot.version(),
            source_snapshot.language().map(|language| language.id())
        );
        state
            .pending
            .retain(|_, job| job.task.upgrade().is_some() || job.ready.upgrade().is_some());
        if let Some(job) = state.pending.get(&key) {
            if let Some(trace) = &trace {
                trace.record("index_join", format_args!("scan_request={}", job.trace_id));
            }
            if let Some(ready) = job.ready.upgrade() {
                return Task::ready(Ok(ready));
            }
            if let Some(task) = job.task.upgrade() {
                return cx.background_spawn(async move {
                    task.await.map_err(|error| anyhow::anyhow!("{error:#}"))
                });
            }
        }
        let scan_trace_id = trace
            .as_ref()
            .map_or_else(String::new, |trace| trace.0.id.clone());
        let build_lock = self.symbol_index.build_lock.clone();
        let settings = DiskLanguageSettings::new(cx);
        let task: SharedIndex = {
            let jobs = jobs.clone();
            let key = key.clone();
            async move {
                let _build_guard = build_lock.lock().await;
                let result = async {
                    let previous = jobs.lock().disk.clone();
                    let disk = if let Some(disk) = previous
                        .as_ref()
                        .filter(|disk| disk.generation == generation)
                    {
                        disk.clone()
                    } else {
                        let disk = Arc::new(
                            build_disk_index(
                                &*fs,
                                &languages,
                                &settings,
                                &worktrees,
                                previous.as_deref(),
                                generation,
                                trace.as_ref(),
                            )
                            .await,
                        );
                        let mut state = jobs.lock();
                        if state.generation == generation.worktree
                            && state.settings_generation == generation.settings
                            && languages.version() == disk.generation.languages
                        {
                            state.disk = Some(disk.clone());
                        }
                        disk
                    };

                    let source_language = source_snapshot
                        .language()
                        .context("No language for syntax index")?;
                    let source_document = cached_buffer_document(
                        &cache,
                        source_path.as_ref(),
                        &source_snapshot,
                        source_language,
                    )?;
                    let mut overlay_bytes = source_document.memory_cost();
                    let mut overlay_limit_reached = false;
                    let mut files = vec![IndexedFile {
                        path: source_path.clone(),
                        document: source_document,
                    }];
                    let mut seen = HashSet::default();
                    if let Some(path) = &source_path {
                        seen.insert(path.clone());
                    }
                    let mut overlays: Vec<_> = overlays.into_iter().collect();
                    overlays.sort_by(|left, right| left.0.cmp(&right.0));
                    for (path, snapshot) in overlays {
                        if !seen.insert(path.clone()) {
                            continue;
                        }

                        let language = snapshot.language().filter(|language| {
                            SymbolLanguage::for_name(language.name().as_ref()).is_some()
                                && language.grammar().is_some()
                        });
                        // An unsupported editor mode does not hide the file's symbols.
                        let detected;
                        let language = if let Some(language) = language {
                            language
                        } else if let Some(kind) = disk
                            .entries
                            .get(&path)
                            .and_then(|entry| entry.document.as_ref())
                            .map(|document| document.language)
                        {
                            detected = languages.language_for_name(kind.name()).await?;
                            &detected
                        } else {
                            continue;
                        };
                        if snapshot.len() as u64 > MAX_FILE_BYTES {
                            overlay_limit_reached = true;
                            continue;
                        }
                        let document = match cached_buffer_document(
                            &cache,
                            Some(&path),
                            &snapshot,
                            language,
                        ) {
                            Ok(document) => document,
                            Err(error) => {
                                log::warn!("Indexing open buffer {path:?}: {error:#}");
                                continue;
                            }
                        };
                        let bytes = document.memory_cost();
                        if overlay_bytes.saturating_add(bytes) > OVERLAY_INDEX_BYTES {
                            overlay_limit_reached = true;
                            continue;
                        }
                        overlay_bytes += bytes;
                        files.push(IndexedFile {
                            path: Some(path),
                            document,
                        });
                    }

                    files.extend(disk.entries.iter().filter_map(|(path, entry)| {
                        if seen.contains(path) {
                            return None;
                        }
                        Some(IndexedFile {
                            path: Some(path.clone()),
                            document: entry.document.clone()?,
                        })
                    }));
                    files[1..].sort_by(|left, right| left.path.cmp(&right.path));
                    if let Some(trace) = &trace {
                        trace.record(
                            "index_ready",
                            format_args!(
                                "indexed={} disk_limit={} overlay_limit={}",
                                files.len(),
                                disk.limit_reached,
                                overlay_limit_reached
                            ),
                        );
                    }
                    anyhow::Ok(ProjectIndex {
                        files,
                        source: 0,
                        source_snapshot,
                    })
                }
                .await
                .map(Arc::new)
                .map_err(Arc::new);
                if let Ok(index) = &result
                    && let Some(job) = jobs.lock().pending.get_mut(&key)
                {
                    job.ready = Arc::downgrade(index);
                }
                result
            }
        }
        .boxed()
        .shared();
        if let Some(weak_task) = task.downgrade() {
            state.pending.insert(
                key,
                IndexJob {
                    task: weak_task,
                    ready: Weak::new(),
                    trace_id: scan_trace_id,
                },
            );
        }
        drop(state);
        cx.background_spawn(async move { task.await.map_err(|error| anyhow::anyhow!("{error:#}")) })
    }

    pub(super) fn syntax_definitions(
        &self,
        buffer: &Entity<Buffer>,
        offset: usize,
        declarations: bool,
        trace: SymbolQueryTrace,
        cx: &mut Context<Self>,
    ) -> Task<Result<Option<Vec<LocationLink>>>> {
        if self.upstream_client().is_some() {
            let kind = if declarations {
                proto::get_syntax_symbols::Kind::Declaration
            } else {
                proto::get_syntax_symbols::Kind::Definition
            };
            let point = offset.to_point_utf16(&buffer.read(cx).snapshot());
            let task = self.request_remote_syntax_symbols(buffer, offset, kind, trace.clone(), cx);
            let buffer = buffer.clone();
            return cx.spawn(async move |store, cx| {
                let response = task.await?;
                let store = store.upgrade().context("Symbol query store was released")?;
                let locations = if declarations {
                    GetDeclarations { position: point }
                        .response_from_proto(
                            response
                                .declarations
                                .context("Missing remote declarations")?,
                            store,
                            buffer,
                            cx.clone(),
                        )
                        .await?
                } else {
                    GetDefinitions { position: point }
                        .response_from_proto(
                            response.definitions.context("Missing remote definitions")?,
                            store,
                            buffer,
                            cx.clone(),
                        )
                        .await?
                };
                trace.record(
                    "remote_decoded",
                    format_args!("targets={}", locations.len()),
                );
                Ok((!locations.is_empty()).then_some(locations))
            });
        }
        if !self.supports_syntax_symbols(buffer, cx) {
            trace.record(
                "skipped",
                format_args!(
                    "reason=unsupported_host_buffer language={:?} local_host={}",
                    buffer.read(cx).language().map(|language| language.name()),
                    self.as_local().is_some()
                ),
            );
            return Task::ready(Ok(None));
        }
        let task = self.prepare_symbol_query(buffer, offset, false, trace.clone(), cx);
        let buffer = buffer.clone();
        cx.spawn(async move |project, cx| {
            let Some(prepared) = task.await? else {
                return Ok(None);
            };
            let snapshot = prepared.source_snapshot.clone();
            let resolution_trace = trace.clone();
            let resolved = cx
                .background_spawn(async move {
                    let documents: Vec<_> = prepared
                        .files
                        .iter()
                        .map(|file| file.document.clone())
                        .collect();
                    let index = Index::new(&documents);
                    let Some(occurrence) = documents[prepared.source].occurrence_at(offset) else {
                        resolution_trace.record(
                            "symbol_unrecognized",
                            format_args!("offset={} reason=no_identifier_at_position", offset),
                        );
                        return (prepared, Vec::new());
                    };
                    resolution_trace.record(
                        "symbol",
                        format_args!(
                            "name={:?} receiver={:?} scope={:?} type_position={} arity={:?}",
                            bounded_name(&occurrence.name),
                            occurrence
                                .receiver
                                .as_ref()
                                .map(|value| bounded_name(value)),
                            documents[prepared.source].scopes[occurrence.scope].owner,
                            occurrence.type_position,
                            occurrence.arity
                        ),
                    );
                    let (mut locations, diagnostics) =
                        index.resolve_with_diagnostics(prepared.source, &occurrence);
                    resolution_trace.record(
                        "resolved",
                        format_args!("targets={} diagnostics={diagnostics:?}", locations.len()),
                    );
                    let preferred: Vec<_> = locations
                        .iter()
                        .copied()
                        .filter(|location| {
                            documents[location.document].symbols[location.symbol].definition
                                != declarations
                        })
                        .collect();
                    if !preferred.is_empty() {
                        locations = preferred;
                    }
                    let targets = locations
                        .into_iter()
                        .map(|location| {
                            let symbol = &documents[location.document].symbols[location.symbol];
                            (location.document, symbol.range.clone())
                        })
                        .collect::<Vec<_>>();
                    (prepared, targets)
                })
                .await;
            if buffer.read_with(cx, |buffer, _| buffer.version() != *snapshot.version()) {
                trace.record(
                    "discarded",
                    format_args!("reason=source_changed_during_indexing"),
                );
                return Ok(None);
            }
            let (prepared, targets) = resolved;
            let mut links = Vec::new();
            for (document, range) in targets {
                let file = &prepared.files[document];
                let target = if document == prepared.source {
                    buffer.clone()
                } else if let Some(path) = &file.path {
                    let task = project.update(cx, |project, cx| {
                        project
                            .buffer_store()
                            .update(cx, |buffers, cx| buffers.open_buffer(path.clone(), cx))
                    })?;
                    match task.await {
                        Ok(target) => target,
                        Err(error) => {
                            log::warn!("Opening syntax index target {path:?}: {error:#}");
                            continue;
                        }
                    }
                } else {
                    continue;
                };
                let range = target.read_with(cx, |target, _| {
                    if target.text() != file.document.text.as_ref() {
                        trace.record(
                            "target_filtered",
                            format_args!("file={:?} reason=target_content_changed", file.path),
                        );
                        return None;
                    }
                    Some(target.anchor_before(range.start)..target.anchor_after(range.end))
                });
                if let Some(range) = range {
                    trace.record("target", format_args!("file={:?} matched=true", file.path));
                    links.push(LocationLink {
                        origin: None,
                        target: Location {
                            buffer: target,
                            range,
                        },
                    });
                }
            }
            if buffer.read_with(cx, |buffer, _| buffer.version() != *snapshot.version()) {
                return Ok(None);
            }
            Ok((!links.is_empty()).then_some(links))
        })
    }

    pub(super) fn syntax_references(
        &self,
        buffer: &Entity<Buffer>,
        offset: usize,
        trace: SymbolQueryTrace,
        cx: &mut Context<Self>,
    ) -> Task<Result<Option<Vec<Location>>>> {
        if self.upstream_client().is_some() {
            let point = offset.to_point_utf16(&buffer.read(cx).snapshot());
            let task = self.request_remote_syntax_symbols(
                buffer,
                offset,
                proto::get_syntax_symbols::Kind::References,
                trace.clone(),
                cx,
            );
            let buffer = buffer.clone();
            return cx.spawn(async move |store, cx| {
                let response = task.await?;
                let store = store.upgrade().context("Symbol query store was released")?;
                let locations = GetReferences { position: point }
                    .response_from_proto(
                        response.references.context("Missing remote references")?,
                        store,
                        buffer,
                        cx.clone(),
                    )
                    .await?;
                trace.record(
                    "remote_decoded",
                    format_args!("references={}", locations.len()),
                );
                Ok((!locations.is_empty()).then_some(locations))
            });
        }
        if !self.supports_syntax_symbols(buffer, cx) {
            trace.record(
                "skipped",
                format_args!(
                    "reason=unsupported_host_buffer language={:?} local_host={}",
                    buffer.read(cx).language().map(|language| language.name()),
                    self.as_local().is_some()
                ),
            );
            return Task::ready(Ok(None));
        }
        let task = self.prepare_symbol_query(buffer, offset, false, trace.clone(), cx);
        let buffer = buffer.clone();
        cx.spawn(async move |project, cx| {
            let Some(prepared) = task.await? else {
                return Ok(None);
            };
            let source_snapshot = prepared.source_snapshot.clone();
            let resolution_trace = trace.clone();
            let (prepared, references) = cx
                .background_spawn(async move {
                    let documents: Vec<_> = prepared
                        .files
                        .iter()
                        .map(|file| file.document.clone())
                        .collect();
                    let index = Index::new(&documents);
                    let mut references =
                        std::collections::BTreeMap::<usize, Vec<std::ops::Range<usize>>>::new();
                    if let Some(occurrence) = documents[prepared.source].occurrence_at(offset) {
                        resolution_trace.record(
                            "symbol",
                            format_args!(
                                "name={:?} receiver={:?}",
                                bounded_name(&occurrence.name),
                                occurrence.receiver
                            ),
                        );
                        let (resolved, diagnostics) =
                            index.resolve_with_diagnostics(prepared.source, &occurrence);
                        resolution_trace.record(
                            "resolved",
                            format_args!("targets={} diagnostics={diagnostics:?}", resolved.len()),
                        );
                        if let Some(target) = resolved.first().copied().filter(|target| {
                            resolved
                                .iter()
                                .all(|other| index.same_symbol(*target, *other))
                        }) {
                            for (document, file) in documents.iter().enumerate() {
                                for (symbol, declaration) in file.symbols.iter().enumerate() {
                                    if index
                                        .same_symbol(target, SymbolLocation { document, symbol })
                                    {
                                        references
                                            .entry(document)
                                            .or_default()
                                            .push(declaration.range.clone());
                                    }
                                }
                                for occurrence in &file.occurrences {
                                    if occurrence.name != index.name_in_document(document, target) {
                                        continue;
                                    }
                                    let candidates = index.resolve(document, occurrence);
                                    if !candidates.is_empty()
                                        && candidates
                                            .iter()
                                            .all(|candidate| index.same_symbol(target, *candidate))
                                    {
                                        references
                                            .entry(document)
                                            .or_default()
                                            .push(occurrence.range.clone());
                                    }
                                }
                            }
                        }
                    }
                    resolution_trace.record(
                        "resolved",
                        format_args!(
                            "references={} files={}",
                            references.values().map(Vec::len).sum::<usize>(),
                            references.len()
                        ),
                    );
                    (prepared, references)
                })
                .await;
            if buffer.read_with(cx, |buffer, _| {
                buffer.version() != *source_snapshot.version()
            }) {
                return Ok(None);
            }
            let mut locations = Vec::new();
            for (document, ranges) in references {
                let file = &prepared.files[document];
                let target = if document == prepared.source {
                    buffer.clone()
                } else if let Some(path) = &file.path {
                    let task = project.update(cx, |project, cx| {
                        project
                            .buffer_store()
                            .update(cx, |buffers, cx| buffers.open_buffer(path.clone(), cx))
                    })?;
                    match task.await {
                        Ok(target) => target,
                        Err(error) => {
                            log::warn!("Opening syntax index target {path:?}: {error:#}");
                            continue;
                        }
                    }
                } else {
                    continue;
                };
                let ranges = target.read_with(cx, |buffer, _| {
                    if buffer.text() != file.document.text.as_ref() {
                        return Vec::new();
                    }
                    ranges
                        .into_iter()
                        .map(|range| {
                            buffer.anchor_before(range.start)..buffer.anchor_after(range.end)
                        })
                        .collect::<Vec<_>>()
                });
                locations.extend(ranges.into_iter().map(|range| Location {
                    buffer: target.clone(),
                    range,
                }));
            }
            if buffer.read_with(cx, |buffer, _| {
                buffer.version() != *source_snapshot.version()
            }) {
                return Ok(None);
            }
            Ok((!locations.is_empty()).then_some(locations))
        })
    }

    pub(super) fn syntax_completions(
        &self,
        buffer: &Entity<Buffer>,
        offset: usize,
        trace: SymbolQueryTrace,
        cx: &mut Context<Self>,
    ) -> Task<Result<Vec<CompletionResponse>>> {
        if self.upstream_client().is_some() {
            let point = offset.to_point_utf16(&buffer.read(cx).snapshot());
            let task = self.request_remote_syntax_symbols(
                buffer,
                offset,
                proto::get_syntax_symbols::Kind::Completions,
                trace.clone(),
                cx,
            );
            let buffer = buffer.clone();
            return cx.spawn(async move |store, cx| {
                let response = task.await?;
                let store = store.upgrade().context("Symbol query store was released")?;
                let core = GetCompletions {
                    position: point,
                    server_id: None,
                    context: lsp::CompletionContext {
                        trigger_kind: lsp::CompletionTriggerKind::INVOKED,
                        trigger_character: None,
                    },
                }
                .response_from_proto(
                    response.completions.context("Missing remote completions")?,
                    store,
                    buffer.clone(),
                    cx.clone(),
                )
                .await?;
                anyhow::ensure!(
                    response.completion_labels.len() == core.completions.len(),
                    "Remote symbol completion labels do not match candidates"
                );
                let language = buffer.read_with(cx, |buffer, _| buffer.language().cloned());
                let mut completions = crate::lsp_store::populate_labels_for_completions(
                    core.completions,
                    language,
                    None,
                )
                .await;
                for (completion, label) in completions.iter_mut().zip(response.completion_labels) {
                    completion.label = CodeLabel::plain(label, Some(&completion.new_text));
                    completion.match_start = Some(completion.replace_range.start);
                }
                trace.record(
                    "remote_decoded",
                    format_args!("completions={}", completions.len()),
                );
                if completions.is_empty() {
                    return Ok(Vec::new());
                }
                Ok(vec![CompletionResponse {
                    completions,
                    is_incomplete: core.is_incomplete,
                    display_options: Default::default(),
                }])
            });
        }
        if !self.supports_syntax_symbols(buffer, cx) {
            trace.record(
                "skipped",
                format_args!(
                    "reason=unsupported_host_buffer language={:?} local_host={}",
                    buffer.read(cx).language().map(|language| language.name()),
                    self.as_local().is_some()
                ),
            );
            return Task::ready(Ok(Vec::new()));
        }
        let task = self.prepare_symbol_query(buffer, offset, true, trace.clone(), cx);
        let buffer = buffer.clone();
        cx.spawn(async move |_, cx| {
            let Some(prepared) = task.await? else {
                return Ok(Vec::new());
            };
            let source_snapshot = prepared.source_snapshot.clone();
            let resolution_trace = trace.clone();
            let completions = cx
                .background_spawn(async move {
                    let documents: Vec<_> = prepared
                        .files
                        .iter()
                        .map(|file| file.document.clone())
                        .collect();
                    let index = Index::new(&documents);
                    let Some((range, occurrence)) =
                        documents[prepared.source].completion_context(offset)
                    else {
                        return Vec::new();
                    };
                    let replace_range = prepared.source_snapshot.anchor_before(range.start)
                        ..prepared.source_snapshot.anchor_after(range.end);
                    resolution_trace.record(
                        "completion_context",
                        format_args!(
                            "prefix={:?} receiver={:?}",
                            bounded_name(&occurrence.name),
                            occurrence.receiver
                        ),
                    );
                    index
                        .complete(prepared.source, &occurrence)
                        .into_iter()
                        .map(|location| {
                            let symbol = &documents[location.document].symbols[location.symbol];
                            let name = index.name_in_document(prepared.source, location).to_owned();
                            let label = if let Some(signature) = &symbol.signature {
                                format!("{name}({signature})")
                            } else {
                                name.clone()
                            };
                            let label = if symbol.type_name.is_empty() {
                                label
                            } else {
                                format!("{label}  {}", symbol.type_name)
                            };
                            Completion {
                                replace_range: replace_range.clone(),
                                new_text: name.clone(),
                                label: CodeLabel::plain(label, Some(&name)),
                                documentation: None,
                                source: CompletionSource::SyntaxSymbol {
                                    kind: symbol.kind.completion_kind(),
                                },
                                icon_path: None,
                                icon_color: None,
                                match_start: Some(replace_range.start),
                                snippet_deduplication_key: None,
                                insert_text_mode: None,
                                confirm: None,
                                group: None,
                            }
                        })
                        .collect::<Vec<_>>()
                })
                .await;
            if buffer.read_with(cx, |buffer, _| {
                buffer.version() != *source_snapshot.version()
            }) || completions.is_empty()
            {
                return Ok(Vec::new());
            }
            Ok(vec![CompletionResponse {
                completions,
                display_options: Default::default(),
                is_incomplete: true,
            }])
        })
    }
}

fn send_remote_symbol_query(
    client: AnyProtoClient,
    request: proto::GetSyntaxSymbols,
    trace: SymbolQueryTrace,
    cx: &App,
) -> Task<Result<proto::GetSyntaxSymbolsResponse>> {
    let mut cancellation = RemoteQueryCancellation {
        client: client.clone(),
        request: Some(proto::CancelSyntaxSymbols {
            project_id: request.project_id,
            trace_id: request.trace_id.clone(),
        }),
    };
    let project_id = request.project_id;
    let executor = cx.background_executor().clone();
    executor.clone().spawn(async move {
        trace.record("remote_send", format_args!("project_id={project_id}"));
        let response = client
            .request(request)
            .with_timeout(REMOTE_QUERY_TIMEOUT, &executor)
            .await
            .context("Remote symbol index request timed out")?
            .context("Remote symbol index request failed")?;
        cancellation.disarm();
        trace.record(
            "remote_received",
            format_args!(
                "definitions={} declarations={} references={} completions={}",
                response
                    .definitions
                    .as_ref()
                    .map_or(0, |value| value.links.len()),
                response
                    .declarations
                    .as_ref()
                    .map_or(0, |value| value.links.len()),
                response
                    .references
                    .as_ref()
                    .map_or(0, |value| value.locations.len()),
                response
                    .completions
                    .as_ref()
                    .map_or(0, |value| value.completions.len())
            ),
        );
        Ok(response)
    })
}
fn project_path(buffer: &Entity<Buffer>, cx: &App) -> Option<ProjectPath> {
    let file = buffer.read(cx).file()?;
    Some(ProjectPath {
        worktree_id: file.worktree_id(cx),
        path: file.path().clone(),
    })
}

fn cached_buffer_document(
    cache: &Mutex<DocumentCache>,
    path: Option<&ProjectPath>,
    snapshot: &BufferSnapshot,
    language: &Arc<Language>,
) -> Result<Arc<Document>> {
    let stamp = format!(
        "buffer:{:?}:{:?}:{:?}",
        snapshot.remote_id(),
        snapshot.version(),
        language.id(),
    );
    if let Some(document) = path.and_then(|path| cache.lock().get(path, &stamp)) {
        return Ok(document);
    }
    let document = index_snapshot(snapshot, language, path)?;
    if let Some(path) = path {
        cache.lock().insert(path.clone(), stamp, document.clone());
    }
    Ok(document)
}

async fn build_disk_index(
    fs: &dyn fs::Fs,
    languages: &Arc<LanguageRegistry>,
    settings: &DiskLanguageSettings,
    worktrees: &[(worktree::Snapshot, PathBuf)],
    previous: Option<&DiskIndex>,
    mut generation: DiskGeneration,
    trace: Option<&SymbolQueryTrace>,
) -> DiskIndex {
    let mut supported = HashMap::default();
    for name in ["C++", "Java", "Kotlin"] {
        if languages.available_language_for_name(name).is_none() {
            continue;
        }
        match languages.language_for_name(name).await {
            Ok(language) if language.grammar().is_some() => {
                supported.insert(language.id(), language);
            }
            Ok(_) => {}
            Err(error) => log::warn!("Loading {name} grammar for syntax index: {error:#}"),
        }
    }

    generation.languages = languages.version();
    let previous = previous.filter(|previous| {
        previous.generation.settings == generation.settings
            && previous.generation.languages == generation.languages
    });
    let mut entries = HashMap::default();
    let mut bytes = 0usize;
    let mut limit_reached = false;
    let mut scanned = 0usize;
    let mut reads = 0;
    'worktrees: for (worktree, full_root) in worktrees {
        for entry in worktree.files(false, 0) {
            scanned += 1;
            if scanned.is_multiple_of(64) {
                smol::future::yield_now().await;
            }
            if entry.is_external || entry.is_private || entry.is_fifo || entry.size > MAX_FILE_BYTES
            {
                continue;
            }

            let path = ProjectPath {
                worktree_id: worktree.id(),
                path: entry.path.clone(),
            };
            let entry_bytes = std::mem::size_of::<ProjectPath>()
                + std::mem::size_of::<DiskIndexEntry>()
                + path.path.as_unix_str().len();
            if bytes.saturating_add(entry_bytes) > DISK_INDEX_BYTES {
                limit_reached = true;
                break 'worktrees;
            }

            let cached = previous
                .and_then(|index| index.entries.get(&path))
                .filter(|cached| cached.matches(entry));
            let document = if let Some(cached) = cached {
                cached.document.clone()
            } else {
                reads += 1;
                match read_disk_document(
                    fs, languages, &supported, settings, &path, worktree, full_root,
                )
                .await
                {
                    Ok(document) => document,
                    Err(error) => {
                        log::debug!("Indexing {path:?}: {error:#}");
                        continue;
                    }
                }
            };

            let document_bytes = document
                .as_ref()
                .map_or(0, |document| document.memory_cost());
            if bytes
                .saturating_add(entry_bytes)
                .saturating_add(document_bytes)
                > DISK_INDEX_BYTES
            {
                limit_reached = true;
                break 'worktrees;
            }
            bytes += entry_bytes + document_bytes;
            entries.insert(
                path,
                DiskIndexEntry {
                    modified: entry.mtime,
                    size: entry.size,
                    inode: entry.inode,
                    document,
                },
            );
        }
    }
    if limit_reached {
        log::warn!(
            "Syntax index retained {} files before reaching its disk index memory limit",
            entries.len()
        );
    }
    if let Some(trace) = trace {
        trace.record(
            "disk_index_ready",
            format_args!(
                "scanned={scanned} reads={reads} bytes={bytes} limit_reached={limit_reached}"
            ),
        );
    }
    DiskIndex {
        generation,
        entries,
        limit_reached,
    }
}

async fn read_disk_document(
    fs: &dyn fs::Fs,
    languages: &Arc<LanguageRegistry>,
    supported: &HashMap<language::LanguageId, Arc<Language>>,
    settings: &DiskLanguageSettings,
    path: &ProjectPath,
    worktree: &worktree::Snapshot,
    full_root: &std::path::Path,
) -> Result<Option<Arc<Document>>> {
    let path_style = worktree.path_style();
    let relative = path.path.display(path_style);
    let absolute_path = worktree.abs_path().join(relative.as_ref());
    let full_path = full_root.join(relative.as_ref());
    let mut bytes = Vec::new();
    fs.open_sync(&absolute_path)
        .await?
        .take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Ok(None);
    }
    let mut text = file_content::decode_text(bytes)?.text;
    language::LineEnding::normalize(&mut text);
    let content = text::Rope::from(text.as_str());
    let language = settings
        .language(
            languages,
            path,
            &full_path,
            &absolute_path,
            path_style,
            &content,
        )
        .and_then(|id| supported.get(&id));
    language
        .map(|language| index_text(&text, language, Some(path)))
        .transpose()
}

fn index_snapshot(
    snapshot: &BufferSnapshot,
    language: &Arc<Language>,
    path: Option<&ProjectPath>,
) -> Result<Arc<Document>> {
    if snapshot
        .language()
        .is_some_and(|current| current.id() == language.id())
        && let Some(mut document) = snapshot.symbol_index()
    {
        document.path = index_path(path);
        return Ok(Arc::new(document));
    }
    index_text(&snapshot.text(), language, path)
}

fn index_text(
    text: &str,
    language: &Language,
    path: Option<&ProjectPath>,
) -> Result<Arc<Document>> {
    let language_kind = SymbolLanguage::for_name(language.name().as_ref())
        .context("Unsupported syntax symbol language")?;
    let grammar = language
        .grammar()
        .context("No grammar for syntax symbols")?;
    let tree = language::HighlightSource::parse(text, grammar)?;
    let mut document = Document::extract(language_kind, Arc::from(text), tree.root_node());
    document.path = index_path(path);
    Ok(Arc::new(document))
}

fn index_path(path: Option<&ProjectPath>) -> Option<String> {
    path.map(|path| format!("{:?}/{}", path.worktree_id, path.path.as_unix_str()))
}

fn bounded_name(name: &str) -> String {
    name.chars().take(128).collect()
}

#[cfg(test)]
mod tests;
