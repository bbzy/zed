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
use gpui::{App, AppContext as _, Context, Entity, Task};
use language::{
    Buffer, BufferSnapshot, CodeLabel, Language, Location, ToOffset, ToPointUtf16,
    symbol_index::{Document, Index, SymbolLanguage, SymbolLocation},
};
use parking_lot::Mutex;
use rpc::proto;
use std::{
    fmt,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

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
            stage: Mutex::new("request"),
            finished: AtomicBool::new(false),
            started: Instant::now(),
        }))
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
        if !self.finished.load(Ordering::Relaxed) {
            log::log!(target: "project::symbol_index", self.level(), "syntax-symbols request={} role={} operation={} stage=cancelled last_stage={} elapsed_ms={}", self.id, self.role, self.operation, *self.stage.lock(), self.started.elapsed().as_millis());
        }
    }
}

const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
const CACHE_BYTES: usize = 64 * 1024 * 1024;

#[derive(Default)]
pub(super) struct SymbolIndexStore {
    cache: Arc<Mutex<HashMap<ProjectPath, CachedDocument>>>,
    warm_task: Option<Task<()>>,
}

struct CachedDocument {
    stamp: String,
    document: Arc<Document>,
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

#[derive(Default, Debug)]
struct IndexStatistics {
    scanned: usize,
    indexed: usize,
    cache_hits: usize,
    disk_reads: usize,
    read_errors: usize,
    decode_errors: usize,
    analysis_errors: usize,
    skipped_size: usize,
    skipped_external_or_private: usize,
}

impl Project {
    pub fn is_syntax_completion_trigger(
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
        (self.is_local() || self.is_via_remote_server())
            && self
                .lsp_store
                .read(cx)
                .syntax_language_supported(buffer, cx)
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
        cx.background_spawn(async move {
            trace.record("remote_send", format_args!("project_id={project_id}"));
            let response = client.request(request).await.context(
                "Remote symbol index request failed; the remote server must match this client",
            )?;
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
        let result = async {
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
            buffer
                .update(&mut cx, |buffer, _| buffer.wait_for_version(version))
                .await?;
            let offset = buffer.read_with(&cx, |buffer, _| position.to_offset(&buffer.snapshot()));
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
        }
        .await;
        match &result {
            Ok(_) => trace.finish("complete", format_args!("response_ready=true")),
            Err(error) => trace.finish("failed", format_args!("error={error:#}")),
        }
        result
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

    pub(super) fn warm_symbol_index(&mut self, cx: &mut Context<Self>) {
        let Some(buffer) = self
            .buffer_store()
            .read(cx)
            .buffers()
            .collect::<Vec<_>>()
            .into_iter()
            .find(|buffer| self.supports_syntax_symbols(buffer, cx))
        else {
            return;
        };
        let task = self.prepare_symbol_index(&buffer, None, cx);
        self.symbol_index.warm_task = Some(cx.background_spawn(async move {
            if let Err(error) = task.await {
                log::warn!("Building syntax symbol index: {error:#}");
            }
        }));
    }

    fn prepare_symbol_index(
        &self,
        source: &Entity<Buffer>,
        trace: Option<SymbolQueryTrace>,
        cx: &mut Context<Self>,
    ) -> Task<Result<ProjectIndex>> {
        let source_snapshot = source.read(cx).snapshot();
        let source_path = project_path(source, cx);
        let overlays: HashMap<_, _> = self
            .buffer_store()
            .read(cx)
            .buffers()
            .collect::<Vec<_>>()
            .into_iter()
            .filter_map(|buffer| Some((project_path(&buffer, cx)?, buffer.read(cx).snapshot())))
            .collect();
        let worktrees: Vec<_> = self
            .worktree_store()
            .read(cx)
            .worktrees()
            .filter(|worktree| worktree.read(cx).is_local())
            .map(|worktree| worktree.read(cx).snapshot())
            .collect();
        let languages = self.languages.clone();
        let Some(fs) = self.symbol_index_fs() else {
            return Task::ready(Err(anyhow::anyhow!(
                "Syntax indexing requires the project host filesystem"
            )));
        };
        let cache = self.symbol_index.cache.clone();
        cx.background_spawn(async move {
            let mut statistics = IndexStatistics::default();
            let mut last_progress = Instant::now();
            if let Some(trace) = &trace {
                trace.record(
                    "index_start",
                    format_args!("worktrees={} overlays={}", worktrees.len(), overlays.len()),
                );
            }
            let mut files = Vec::new();
            let mut seen = HashSet::default();
            let mut active_languages: HashMap<_, Arc<Language>> = HashMap::default();
            for snapshot in std::iter::once(&source_snapshot).chain(overlays.values()) {
                if let Some(language) = snapshot.language().filter(|language| {
                    SymbolLanguage::for_name(language.name().as_ref()).is_some()
                        && language.grammar().is_some()
                }) {
                    active_languages.insert(language.id(), language.clone());
                }
            }
            for name in ["C++", "Java", "Kotlin"] {
                if languages.available_language_for_name(name).is_none() {
                    continue;
                }
                match languages.language_for_name(name).await {
                    Ok(language) => {
                        if language.grammar().is_some() {
                            active_languages.insert(language.id(), language);
                        }
                    }
                    Err(error) => {
                        log::warn!("Loading {name} grammar for syntax index: {error:#}");
                    }
                }
            }
            let source_language = source_snapshot
                .language()
                .context("No language for syntax index")?;
            let source_stamp = format!(
                "buffer:{:?}:{:?}:{:?}",
                source_snapshot.remote_id(),
                source_snapshot.version(),
                source_language.id()
            );
            let source_document = source_path.as_ref().and_then(|path| {
                cache
                    .lock()
                    .get(path)
                    .filter(|cached| cached.stamp == source_stamp)
                    .map(|cached| cached.document.clone())
            });
            statistics.cache_hits += usize::from(source_document.is_some());
            let source_document = match source_document {
                Some(document) => document,
                None => {
                    let document =
                        index_snapshot(&source_snapshot, source_language, source_path.as_ref())?;
                    if let Some(path) = &source_path {
                        cache.lock().insert(
                            path.clone(),
                            CachedDocument {
                                stamp: source_stamp,
                                document: document.clone(),
                            },
                        );
                    }
                    document
                }
            };
            files.push(IndexedFile {
                path: source_path.clone(),
                document: source_document,
            });
            if let Some(trace) = &trace {
                trace.record(
                    "source_ready",
                    format_args!(
                        "file={:?} symbols={} occurrences={} bytes={}",
                        source_path,
                        files[0].document.symbols.len(),
                        files[0].document.occurrences.len(),
                        files[0].document.text.len()
                    ),
                );
            }
            if let Some(path) = &source_path {
                seen.insert(path.clone());
            }

            // Open buffers take precedence even when their disk entry is missing or ignored.
            for (path, snapshot) in &overlays {
                if seen.contains(path) {
                    continue;
                }
                seen.insert(path.clone());
                let Some(language) = snapshot.language().filter(|language| {
                    SymbolLanguage::for_name(language.name().as_ref()).is_some()
                        && language.grammar().is_some()
                }) else {
                    continue;
                };
                let stamp = format!(
                    "buffer:{:?}:{:?}:{:?}",
                    snapshot.remote_id(),
                    snapshot.version(),
                    language.id()
                );
                let document = cache
                    .lock()
                    .get(path)
                    .filter(|cached| cached.stamp == stamp)
                    .map(|cached| cached.document.clone());
                statistics.cache_hits += usize::from(document.is_some());
                let document = match document {
                    Some(document) => document,
                    None => {
                        let document = match index_snapshot(snapshot, language, Some(path)) {
                            Ok(document) => document,
                            Err(error) => {
                                log::warn!("Indexing open buffer {:?}: {error:#}", path.path);
                                continue;
                            }
                        };
                        cache.lock().insert(
                            path.clone(),
                            CachedDocument {
                                stamp,
                                document: document.clone(),
                            },
                        );
                        document
                    }
                };
                files.push(IndexedFile {
                    path: Some(path.clone()),
                    document,
                });
                seen.insert(path.clone());
            }

            let mut live_paths = seen.clone();
            for worktree in worktrees {
                for entry in worktree.files(false, 0) {
                    statistics.scanned += 1;
                    if statistics.scanned.is_multiple_of(256)
                        && last_progress.elapsed().as_secs() >= 1
                    {
                        if let Some(trace) = &trace {
                            trace.record(
                                "index_progress",
                                format_args!(
                                    "scanned={} indexed={} disk_reads={} cache_hits={}",
                                    statistics.scanned,
                                    files.len(),
                                    statistics.disk_reads,
                                    statistics.cache_hits
                                ),
                            );
                        }
                        last_progress = Instant::now();
                    }
                    if entry.is_external || entry.is_private || entry.is_fifo {
                        statistics.skipped_external_or_private += 1;
                        continue;
                    }
                    if entry.size > MAX_FILE_BYTES {
                        statistics.skipped_size += 1;
                        continue;
                    }
                    let path = ProjectPath {
                        worktree_id: worktree.id(),
                        path: entry.path.clone(),
                    };
                    live_paths.insert(path.clone());
                    if seen.contains(&path) {
                        continue;
                    }
                    let absolute_path = worktree.abs_path().join(entry.path.as_std_path());
                    let Some(language_id) = languages.language_for_file_path(&absolute_path) else {
                        continue;
                    };
                    let Some(language) = active_languages.get(&language_id) else {
                        continue;
                    };
                    let stamp = format!(
                        "disk:{:?}:{}:{}:{:?}",
                        entry.mtime,
                        entry.size,
                        entry.inode,
                        language.id()
                    );
                    let document = cache
                        .lock()
                        .get(&path)
                        .filter(|cached| cached.stamp == stamp && entry.mtime.is_some())
                        .map(|cached| cached.document.clone());
                    statistics.cache_hits += usize::from(document.is_some());
                    let document = match document {
                        Some(document) => document,
                        None => {
                            statistics.disk_reads += 1;
                            let bytes = match fs.load_bytes(&absolute_path).await {
                                Ok(bytes) => bytes,
                                Err(error) => {
                                    statistics.read_errors += 1;
                                    log::warn!(
                                        "Reading syntax index file {}: {error:#}",
                                        absolute_path.display()
                                    );
                                    continue;
                                }
                            };
                            if bytes.len() as u64 > MAX_FILE_BYTES {
                                statistics.skipped_size += 1;
                                continue;
                            }
                            let mut text = match file_content::decode_text(bytes) {
                                Ok(decoded) => decoded.text,
                                Err(error) => {
                                    statistics.decode_errors += 1;
                                    log::warn!(
                                        "Decoding syntax index file {}: {error:#}",
                                        absolute_path.display()
                                    );
                                    continue;
                                }
                            };
                            language::LineEnding::normalize(&mut text);
                            let document = match index_text(&text, language, Some(&path)) {
                                Ok(document) => document,
                                Err(error) => {
                                    statistics.analysis_errors += 1;
                                    log::warn!("Indexing {}: {error:#}", absolute_path.display());
                                    continue;
                                }
                            };
                            cache.lock().insert(
                                path.clone(),
                                CachedDocument {
                                    stamp,
                                    document: document.clone(),
                                },
                            );
                            document
                        }
                    };
                    files.push(IndexedFile {
                        path: Some(path.clone()),
                        document,
                    });
                    seen.insert(path);
                }
            }
            files[1..].sort_by(|left, right| left.path.cmp(&right.path));
            let mut cache = cache.lock();
            cache.retain(|path, _| live_paths.contains(path));
            let cache_bytes = cache
                .values()
                .map(|cached| cached.document.memory_cost())
                .sum::<usize>();
            if cache_bytes > CACHE_BYTES {
                // Query snapshots own their documents independently of the reusable cache.
                cache.clear();
            }
            statistics.indexed = files.len();
            if let Some(trace) = &trace {
                trace.record(
                    "index_ready",
                    format_args!(
                        "statistics={statistics:?} cache_bytes={} cache_evicted={}",
                        cache_bytes,
                        cache_bytes > CACHE_BYTES
                    ),
                );
            }
            Ok(ProjectIndex {
                files,
                source: 0,
                source_snapshot,
            })
        })
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
        let task = self.prepare_symbol_index(buffer, Some(trace.clone()), cx);
        let buffer = buffer.clone();
        cx.spawn(async move |project, cx| {
            let prepared = task.await?;
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
                    project
                        .update(cx, |project, cx| {
                            project
                                .buffer_store()
                                .update(cx, |buffers, cx| buffers.open_buffer(path.clone(), cx))
                        })?
                        .await?
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
        let task = self.prepare_symbol_index(buffer, Some(trace.clone()), cx);
        let buffer = buffer.clone();
        cx.spawn(async move |project, cx| {
            let prepared = task.await?;
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
                    project
                        .update(cx, |project, cx| {
                            project
                                .buffer_store()
                                .update(cx, |buffers, cx| buffers.open_buffer(path.clone(), cx))
                        })?
                        .await?
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
        let task = self.prepare_symbol_index(buffer, Some(trace.clone()), cx);
        let buffer = buffer.clone();
        cx.spawn(async move |_, cx| {
            let prepared = task.await?;
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

fn project_path(buffer: &Entity<Buffer>, cx: &App) -> Option<ProjectPath> {
    let file = buffer.read(cx).file()?;
    Some(ProjectPath {
        worktree_id: file.worktree_id(cx),
        path: file.path().clone(),
    })
}

fn index_snapshot(
    snapshot: &BufferSnapshot,
    language: &Arc<Language>,
    path: Option<&ProjectPath>,
) -> Result<Arc<Document>> {
    if let Some(mut document) = snapshot.symbol_index() {
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
