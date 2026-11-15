use crate::{Completion, CompletionResponse, CompletionSource, LocationLink, Project, ProjectPath};
use anyhow::{Context as _, Result};
use collections::{HashMap, HashSet};
use gpui::{App, AppContext as _, Context, Entity, Task};
use language::{
    Buffer, BufferSnapshot, CodeLabel, Language, Location,
    symbol_index::{Document, Index, SymbolLanguage, SymbolLocation},
};
use parking_lot::Mutex;
use std::sync::Arc;

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
        self.is_local()
            && !self.is_via_remote_server()
            && buffer.read(cx).language().is_some_and(|language| {
                SymbolLanguage::for_name(language.name().as_ref()).is_some()
                    && language.grammar().is_some()
            })
    }

    pub(super) fn warm_symbol_index(&mut self, cx: &mut Context<Self>) {
        let Some(buffer) = self
            .opened_buffers(cx)
            .into_iter()
            .find(|buffer| self.supports_syntax_symbols(buffer, cx))
        else {
            return;
        };
        let task = self.prepare_symbol_index(&buffer, cx);
        self.symbol_index.warm_task = Some(cx.background_spawn(async move {
            if let Err(error) = task.await {
                log::warn!("Building syntax symbol index: {error:#}");
            }
        }));
    }

    fn prepare_symbol_index(
        &self,
        source: &Entity<Buffer>,
        cx: &mut Context<Self>,
    ) -> Task<Result<ProjectIndex>> {
        let source_snapshot = source.read(cx).snapshot();
        let source_path = project_path(source, cx);
        let overlays: HashMap<_, _> = self
            .opened_buffers(cx)
            .into_iter()
            .filter_map(|buffer| Some((project_path(&buffer, cx)?, buffer.read(cx).snapshot())))
            .collect();
        let worktrees: Vec<_> = self
            .worktrees(cx)
            .filter(|worktree| worktree.read(cx).is_local())
            .map(|worktree| worktree.read(cx).snapshot())
            .collect();
        let languages = self.languages.clone();
        let fs = self.fs.clone();
        let cache = self.symbol_index.cache.clone();
        cx.background_spawn(async move {
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
                for entry in worktree.files(false, 0).filter(|entry| {
                    !entry.is_external
                        && !entry.is_private
                        && !entry.is_fifo
                        && entry.size <= MAX_FILE_BYTES
                }) {
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
                    let document = match document {
                        Some(document) => document,
                        None => {
                            let bytes = match fs.load_bytes(&absolute_path).await {
                                Ok(bytes) => bytes,
                                Err(error) => {
                                    log::warn!(
                                        "Reading syntax index file {}: {error:#}",
                                        absolute_path.display()
                                    );
                                    continue;
                                }
                            };
                            if bytes.len() as u64 > MAX_FILE_BYTES {
                                continue;
                            }
                            let mut text = match file_content::decode_text(bytes) {
                                Ok(decoded) => decoded.text,
                                Err(error) => {
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
            if cache
                .values()
                .map(|cached| cached.document.memory_cost())
                .sum::<usize>()
                > CACHE_BYTES
            {
                // Query snapshots own their documents independently of the reusable cache.
                cache.clear();
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
        cx: &mut Context<Self>,
    ) -> Task<Result<Option<Vec<LocationLink>>>> {
        if !self.supports_syntax_symbols(buffer, cx) {
            return Task::ready(Ok(None));
        }
        let task = self.prepare_symbol_index(buffer, cx);
        let buffer = buffer.clone();
        cx.spawn(async move |project, cx| {
            let prepared = task.await?;
            let snapshot = prepared.source_snapshot.clone();
            let resolved = cx
                .background_spawn(async move {
                    let documents: Vec<_> = prepared
                        .files
                        .iter()
                        .map(|file| file.document.clone())
                        .collect();
                    let index = Index::new(&documents);
                    let Some(occurrence) = documents[prepared.source].occurrence_at(offset) else {
                        return (prepared, Vec::new());
                    };
                    let mut locations = index.resolve(prepared.source, &occurrence);
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
                        .update(cx, |project, cx| project.open_buffer(path.clone(), cx))?
                        .await?
                } else {
                    continue;
                };
                let range = target.read_with(cx, |target, _| {
                    if target.text() != file.document.text.as_ref() {
                        return None;
                    }
                    Some(target.anchor_before(range.start)..target.anchor_after(range.end))
                });
                if let Some(range) = range {
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
        cx: &mut Context<Self>,
    ) -> Task<Result<Option<Vec<Location>>>> {
        if !self.supports_syntax_symbols(buffer, cx) {
            return Task::ready(Ok(None));
        }
        let task = self.prepare_symbol_index(buffer, cx);
        let buffer = buffer.clone();
        cx.spawn(async move |project, cx| {
            let prepared = task.await?;
            let source_snapshot = prepared.source_snapshot.clone();
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
                        let resolved = index.resolve(prepared.source, &occurrence);
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
                        .update(cx, |project, cx| project.open_buffer(path.clone(), cx))?
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
        cx: &mut Context<Self>,
    ) -> Task<Result<Vec<CompletionResponse>>> {
        if !self.supports_syntax_symbols(buffer, cx) {
            return Task::ready(Ok(Vec::new()));
        }
        let task = self.prepare_symbol_index(buffer, cx);
        let buffer = buffer.clone();
        cx.spawn(async move |_, cx| {
            let prepared = task.await?;
            let source_snapshot = prepared.source_snapshot.clone();
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
