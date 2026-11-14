use crate::{
    BlameLine, CategoryCommit, ChangedFile, ComparisonBoundary, DiffScope, FileConflict, FileEdit,
    FileInformation, HistoryMode, HistoryPage, Revision, Snapshot, remote::Read,
};
use anyhow::Result;
use serde::{Serialize, de::DeserializeOwned};
use std::{
    collections::HashMap,
    future::Future,
    sync::{Arc, Mutex, Weak},
};

type Flight = smol::lock::Mutex<Option<Result<Arc<Vec<u8>>, Arc<str>>>>;

const MAX_ENTRIES: usize = 512;
const MAX_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct Key {
    operation: String,
    query: Vec<u8>,
}

#[derive(Clone, Debug)]
struct Entry {
    bytes: Arc<Vec<u8>>,
    accessed: u64,
    requested: u64,
    size: usize,
}

#[derive(Debug, Default)]
struct State {
    entries: HashMap<Key, Entry>,
    flights: HashMap<Key, Weak<Flight>>,
    bytes: usize,
    clock: u64,
}

#[derive(Default)]
pub(crate) struct ReadCache(Mutex<State>);

impl std::fmt::Debug for ReadCache {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.state();
        formatter
            .debug_struct("ReadCache")
            .field("entries", &state.entries.len())
            .field("bytes", &state.bytes)
            .finish_non_exhaustive()
    }
}

impl ReadCache {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(|error| {
            log::error!("KK read cache lock was poisoned: {error}");
            error.into_inner()
        })
    }

    fn get(&self, key: &Key) -> Option<Arc<Vec<u8>>> {
        let mut state = self.state();
        state.clock += 1;
        let clock = state.clock;
        let entry = state.entries.get_mut(key)?;
        entry.accessed = clock;
        Some(entry.bytes.clone())
    }

    fn insert(&self, key: Key, bytes: Arc<Vec<u8>>, requested: u64) {
        let size = bytes.len() + key.operation.len() + key.query.len();
        if size > MAX_BYTES {
            return;
        }
        let mut state = self.state();
        state.clock = state.clock.max(requested) + 1;
        let accessed = state.clock;
        if let Some(previous) = state.entries.insert(
            key,
            Entry {
                bytes,
                accessed,
                requested,
                size,
            },
        ) {
            state.bytes -= previous.size;
        }
        state.bytes += size;
        while state.bytes > MAX_BYTES || state.entries.len() > MAX_ENTRIES {
            let oldest = state
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.accessed)
                .map(|(key, _)| key.clone());
            if let Some(oldest) = oldest.and_then(|key| state.entries.remove(&key)) {
                state.bytes -= oldest.size;
            } else {
                break;
            }
        }
    }

    fn flight(&self, key: &Key) -> Arc<Flight> {
        let mut state = self.state();
        state.flights.retain(|_, flight| flight.strong_count() > 0);
        if let Some(flight) = state.flights.get(key).and_then(Weak::upgrade) {
            return flight;
        }
        let flight = Arc::new(smol::lock::Mutex::new(None));
        state.flights.insert(key.clone(), Arc::downgrade(&flight));
        flight
    }

    pub(crate) fn extend(&self, other: &Self) {
        if std::ptr::eq(self, other) {
            return;
        }
        let entries = other.state().entries.clone();
        for (key, entry) in entries {
            if self.get(&key).is_none() {
                self.insert(key, entry.bytes, entry.requested);
            }
        }
    }

    pub(crate) fn clear(&self) {
        let mut state = self.state();
        state.entries.clear();
        state.flights.clear();
        state.bytes = 0;
    }
}

fn immutable_query(query: &Read) -> bool {
    let commit = |revision: &str| {
        matches!(revision.len(), 40 | 64) && revision.bytes().all(|byte| byte.is_ascii_hexdigit())
    };
    let scope = |scope: &DiffScope| match scope {
        DiffScope::Change(revision) => commit(revision),
        DiffScope::Comparison { base, target } => commit(base) && commit(target),
    };
    match query {
        Read::FileContent { revision, .. } | Read::FileContentIfExists { revision, .. } => {
            commit(revision)
        }
        Read::ChangedFiles { scope: value }
        | Read::Diff { scope: value, .. }
        | Read::SearchData { scope: value } => scope(value),
        _ => false,
    }
}

impl Snapshot {
    fn cache_key(&self, query: &Read) -> Result<Key> {
        Ok(Key {
            operation: if immutable_query(query) {
                String::new()
            } else {
                self.operation().to_owned()
            },
            query: crate::remote::encode(&(
                self.repository().root(),
                self.repository().executable(),
                self.repository().remote_cache_identity(),
                query,
            ))?,
        })
    }

    /// Returns only data from this exact snapshot; it never captures the working copy.
    pub fn cached<T: DeserializeOwned>(&self, query: &Read) -> Option<T> {
        if self.repository().check_lifetime().is_err() {
            return None;
        }
        let key = self
            .cache_key(query)
            .inspect_err(|error| log::error!("Could not build KK cache key: {error:#}"))
            .ok()?;
        let bytes = self.repository().read_cache.get(&key)?;
        crate::remote::decode(&bytes)
            .inspect_err(|error| log::error!("Could not decode KK cache: {error:#}"))
            .ok()
    }

    /// Last successful presentation for this query, tagged with its original snapshot.
    /// Callers must still load the current snapshot before treating it as confirmed.
    pub fn previously_cached<T: DeserializeOwned>(&self, query: &Read) -> Option<(Self, T)> {
        self.repository().check_lifetime().ok()?;
        let key = self
            .cache_key(query)
            .inspect_err(|error| log::error!("Could not build KK cache key: {error:#}"))
            .ok()?;
        let found = {
            let cache = self.repository().read_cache.state();
            cache
                .entries
                .iter()
                .filter(|(candidate, _)| candidate.query == key.query)
                .max_by_key(|(_, entry)| entry.requested)
                .map(|(key, _)| key.clone())
        }?;
        let bytes = self.repository().read_cache.get(&found)?;
        let snapshot = if found.operation.is_empty() {
            self.clone()
        } else {
            self.at_operation(found.operation)
        };
        Some((
            snapshot,
            crate::remote::decode(&bytes)
                .inspect_err(|error| {
                    log::error!("Could not decode KK presentation cache: {error:#}")
                })
                .ok()?,
        ))
    }

    pub(crate) async fn read_cached<T: Serialize + DeserializeOwned>(
        &self,
        query: Read,
        load: impl Future<Output = Result<T>>,
    ) -> Result<T> {
        if self.repository().check_lifetime().is_err() {
            return load.await;
        }
        // The default log revset is configuration, so it can change without a new operation.
        // Keep its last presentation available while validating the configured query again.
        let revalidate = matches!(
            &query,
            Read::History {
                mode: HistoryMode::Log,
                ..
            } | Read::HistoryContaining {
                mode: HistoryMode::Log,
                ..
            } | Read::Revisions { revset: None, .. }
                | Read::FileConflict { .. }
                | Read::FileEdit { .. }
        );
        let key = self.cache_key(&query)?;
        let cache = &self.repository().read_cache;
        if !revalidate && let Some(bytes) = cache.get(&key) {
            return crate::remote::decode(&bytes);
        }
        let flight = cache.flight(&key);
        let mut result = flight.lock().await;
        if self.repository().check_lifetime().is_err() {
            return load.await;
        }
        if let Some(result) = result.as_ref() {
            return match result {
                Ok(bytes) => crate::remote::decode(bytes),
                Err(message) => Err(anyhow::anyhow!("{message}")),
            };
        }
        if !revalidate && let Some(bytes) = cache.get(&key) {
            return crate::remote::decode(&bytes);
        }
        let requested = {
            let mut state = cache.state();
            state.clock += 1;
            state.clock
        };
        let value = match load.await {
            Ok(value) => value,
            Err(error) => {
                *result = Some(Err(Arc::from(format!("{error:#}"))));
                return Err(error);
            }
        };
        if self.repository().check_lifetime().is_err() {
            return Ok(value);
        }
        match crate::remote::encode(&value) {
            Ok(bytes) => {
                let bytes = Arc::new(bytes);
                cache.insert(key, bytes.clone(), requested);
                *result = Some(Ok(bytes));
            }
            Err(error) => log::error!("Could not cache KK read: {error:#}"),
        }
        Ok(value)
    }
}

// Queries compose across local and in-process remote reads. Boxing the inner
// futures keeps nested query state from exhausting the executor thread's stack.
impl Snapshot {
    pub async fn current_file_path(&self, revision: &str, path: &str) -> Result<Option<String>> {
        self.read_cached(
            Read::CurrentFilePath {
                revision: revision.into(),
                path: path.into(),
            },
            Box::pin(self.current_file_path_uncached(revision, path)),
        )
        .await
    }

    pub async fn revisions(&self, revset: Option<&str>, limit: usize) -> Result<Vec<Revision>> {
        self.read_cached(
            Read::Revisions {
                revset: revset.map(str::to_owned),
                limit,
            },
            Box::pin(self.revisions_uncached(revset, limit)),
        )
        .await
    }

    pub async fn history(&self, mode: HistoryMode, visible_limit: usize) -> Result<HistoryPage> {
        self.read_cached(
            Read::History {
                mode,
                limit: visible_limit,
            },
            Box::pin(self.history_uncached(mode, visible_limit)),
        )
        .await
    }

    pub async fn commit_emptiness(&self, commit_ids: &[String]) -> Result<HashMap<String, bool>> {
        self.read_cached(
            Read::CommitEmptiness {
                commits: commit_ids.to_vec(),
            },
            Box::pin(self.commit_emptiness_uncached(commit_ids)),
        )
        .await
    }

    pub async fn comparison_candidates(&self) -> Result<Vec<Revision>> {
        self.read_cached(
            Read::ComparisonCandidates,
            Box::pin(self.comparison_candidates_uncached()),
        )
        .await
    }

    pub async fn resolve_revision(&self, previous: &Revision) -> Result<Revision> {
        self.read_cached(
            Read::ResolveRevision {
                revision: previous.clone(),
            },
            Box::pin(self.resolve_revision_uncached(previous)),
        )
        .await
    }

    pub async fn history_containing(
        &self,
        mode: HistoryMode,
        visible_limit: usize,
        targets: &[Revision],
    ) -> Result<(HistoryPage, Vec<Revision>)> {
        self.read_cached(
            Read::HistoryContaining {
                mode,
                limit: visible_limit,
                targets: targets.to_vec(),
            },
            Box::pin(self.history_containing_uncached(mode, visible_limit, targets)),
        )
        .await
    }

    pub async fn search_revisions(
        &self,
        query: &str,
        scope: Option<&str>,
        include_working_copy: bool,
        limit: usize,
    ) -> Result<Vec<Revision>> {
        self.read_cached(
            Read::SearchRevisions {
                query: query.into(),
                scope: scope.map(str::to_owned),
                working_copy: include_working_copy,
                limit,
            },
            Box::pin(self.search_revisions_uncached(query, scope, include_working_copy, limit)),
        )
        .await
    }

    pub async fn comparison_boundary(
        &self,
        target: &str,
        range_start: Option<&str>,
    ) -> Result<ComparisonBoundary> {
        self.read_cached(
            Read::ComparisonBoundary {
                target: target.into(),
                range_start: range_start.map(str::to_owned),
            },
            Box::pin(self.comparison_boundary_uncached(target, range_start)),
        )
        .await
    }

    pub async fn changed_files(&self, scope: &DiffScope) -> Result<Vec<ChangedFile>> {
        self.read_cached(
            Read::ChangedFiles {
                scope: scope.clone(),
            },
            Box::pin(self.changed_files_uncached(scope)),
        )
        .await
    }

    pub async fn diff(&self, scope: &DiffScope, path: Option<&str>) -> Result<Vec<u8>> {
        self.read_cached(
            Read::Diff {
                scope: scope.clone(),
                path: path.map(str::to_owned),
            },
            Box::pin(self.diff_uncached(scope, path)),
        )
        .await
    }

    pub async fn file_content(&self, revision: &str, path: &str) -> Result<Vec<u8>> {
        self.read_cached(
            Read::FileContent {
                revision: revision.into(),
                path: path.into(),
            },
            Box::pin(self.file_content_uncached(revision, path)),
        )
        .await
    }

    pub async fn file_content_if_exists(
        &self,
        revision: &str,
        path: &str,
    ) -> Result<Option<Vec<u8>>> {
        self.read_cached(
            Read::FileContentIfExists {
                revision: revision.into(),
                path: path.into(),
            },
            Box::pin(self.file_content_if_exists_uncached(revision, path)),
        )
        .await
    }

    pub async fn search_data(
        &self,
        scope: &DiffScope,
    ) -> Result<(Vec<crate::ChangedLine>, Vec<ChangedFile>)> {
        self.read_cached(
            Read::SearchData {
                scope: scope.clone(),
            },
            Box::pin(self.search_data_uncached(scope)),
        )
        .await
    }

    pub async fn annotate(&self, revision: &str, path: &str) -> Result<Vec<BlameLine>> {
        self.read_cached(
            Read::Annotate {
                revision: revision.into(),
                path: path.into(),
            },
            Box::pin(self.annotate_uncached(revision, path)),
        )
        .await
    }

    pub async fn file_information(&self, revision: &str, path: &str) -> Result<FileInformation> {
        self.read_cached(
            Read::FileInformation {
                revision: revision.into(),
                path: path.into(),
            },
            Box::pin(self.file_information_uncached(revision, path)),
        )
        .await
    }

    pub async fn conflicted_paths(&self, revision: &str) -> Result<Vec<String>> {
        self.read_cached(
            Read::ConflictedPaths {
                revision: revision.into(),
            },
            Box::pin(self.conflicted_paths_uncached(revision)),
        )
        .await
    }

    pub async fn file_conflict(&self, revision: &str, path: &str) -> Result<FileConflict> {
        self.read_cached(
            Read::FileConflict {
                revision: revision.into(),
                path: path.into(),
            },
            Box::pin(self.file_conflict_uncached(revision, path)),
        )
        .await
    }

    pub async fn category_commits(&self, range_start: &Revision) -> Result<Vec<CategoryCommit>> {
        self.read_cached(
            Read::CategoryCommits {
                range_start: range_start.clone(),
            },
            Box::pin(self.category_commits_uncached(range_start)),
        )
        .await
    }

    pub async fn exact_revision(&self, revset: &str) -> Result<Revision> {
        self.read_cached(
            Read::ExactRevision {
                revset: revset.into(),
            },
            Box::pin(self.exact_revision_uncached(revset)),
        )
        .await
    }

    pub async fn file_edit(&self, revision: &str, path: &str) -> Result<FileEdit> {
        self.read_cached(
            Read::FileEdit {
                revision: revision.into(),
                path: path.into(),
            },
            Box::pin(self.file_edit_uncached(revision, path)),
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Diagnostics, Repository, RepositoryLifetime};
    use std::{
        path::Path,
        sync::atomic::{AtomicUsize, Ordering},
    };

    fn snapshot() -> Snapshot {
        Repository::discovered(
            "/cache-test".into(),
            Path::new("kk"),
            Diagnostics::default(),
            None,
        )
        .snapshot_at("aa".into())
        .expect("snapshot")
        .with_lifetime(Some(RepositoryLifetime::default()))
    }

    #[test]
    fn read_cache_reuses_empty_results_and_separates_versions_and_arguments() {
        smol::block_on(async {
            let snapshot = snapshot();
            let query = Read::FileContent {
                revision: "@".into(),
                path: "empty".into(),
            };
            let calls = AtomicUsize::new(0);
            for _ in 0..2 {
                let result = snapshot
                    .read_cached(query.clone(), async {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Ok(Vec::<u8>::new())
                    })
                    .await
                    .expect("empty read");
                assert!(result.is_empty());
            }
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert_eq!(snapshot.cached::<Vec<u8>>(&query), Some(Vec::new()));
            let newer = snapshot.at_operation("bb".into());
            assert!(newer.cached::<Vec<u8>>(&query).is_none());
            assert!(
                snapshot
                    .cached::<Vec<u8>>(&Read::FileContent {
                        revision: "@".into(),
                        path: "other".into()
                    })
                    .is_none()
            );
            newer
                .read_cached(query.clone(), async { Ok(vec![1u8]) })
                .await
                .expect("new version");
            assert_eq!(snapshot.cached::<Vec<u8>>(&query), Some(Vec::new()));
            assert_eq!(newer.cached::<Vec<u8>>(&query), Some(vec![1]));
        });
    }

    #[test]
    fn read_cache_coalesces_concurrent_reads_and_retries_failures() {
        smol::block_on(async {
            let snapshot = snapshot();
            let calls = AtomicUsize::new(0);
            let query = Read::ComparisonCandidates;
            let (started, running) = smol::channel::bounded::<()>(1);
            let (release, released) = smol::channel::bounded::<()>(1);
            let first = snapshot.read_cached(query.clone(), async {
                calls.fetch_add(1, Ordering::SeqCst);
                started.send(()).await?;
                released.recv().await?;
                Ok(vec![1u8])
            });
            let second = async {
                running.recv().await.expect("read started");
                let second = snapshot.read_cached(query.clone(), async {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(vec![2u8])
                });
                let release = async {
                    release.send(()).await.expect("release read");
                };
                let (result, ()) = futures::join!(second, release);
                result
            };
            let (first, second) = futures::join!(first, second);
            assert_eq!(first.expect("first"), vec![1]);
            assert_eq!(second.expect("second"), vec![1]);
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            let query = Read::Revisions {
                revset: None,
                limit: 5,
            };
            assert!(
                snapshot
                    .read_cached::<Vec<u8>>(query.clone(), async {
                        anyhow::bail!("injected failure")
                    })
                    .await
                    .is_err()
            );
            assert!(snapshot.cached::<Vec<u8>>(&query).is_none());
            assert_eq!(
                snapshot
                    .read_cached(query, async { Ok(vec![3u8]) })
                    .await
                    .expect("retry"),
                vec![3]
            );
        });
    }

    #[test]
    fn read_cache_cancellation_releases_flight_and_closed_lifetime_discards_cache() {
        smol::block_on(async {
            use futures::{FutureExt as _, pin_mut, poll};
            let snapshot = snapshot();
            let query = Read::ComparisonCandidates;
            let first = snapshot.read_cached::<Vec<u8>>(query.clone(), futures::future::pending());
            let mut first = Box::pin(first);
            assert!(poll!(first.as_mut()).is_pending());
            drop(first);
            assert_eq!(
                snapshot
                    .read_cached(query.clone(), async { Ok(vec![4u8]) })
                    .await
                    .expect("after cancellation"),
                vec![4]
            );
            let newer = snapshot.at_operation("bb".into());
            let (release, released) = smol::channel::bounded::<()>(1);
            let pending = newer
                .read_cached(query.clone(), async {
                    released.recv().await?;
                    Ok(vec![5u8])
                })
                .fuse();
            pin_mut!(pending);
            assert!(poll!(pending.as_mut()).is_pending());
            snapshot
                .repository()
                .lifetime()
                .expect("lifetime")
                .invalidate();
            release.send(()).await.expect("release");
            assert_eq!(pending.await.expect("readonly completion"), vec![5]);
            assert!(snapshot.cached::<Vec<u8>>(&query).is_none());
            assert!(newer.cached::<Vec<u8>>(&query).is_none());
            assert!(snapshot.repository().read_cache.state().entries.is_empty());
        });
    }

    #[test]
    fn read_cache_is_bounded_and_preserves_recently_used_entries() {
        let cache = ReadCache::default();
        let key = |index: usize| Key {
            operation: format!("{index:x}"),
            query: vec![0],
        };
        for index in 0..MAX_ENTRIES {
            cache.insert(key(index), Arc::new(vec![1]), index as u64);
        }
        assert!(cache.get(&key(0)).is_some());
        cache.insert(key(MAX_ENTRIES), Arc::new(vec![2]), MAX_ENTRIES as u64);
        assert!(cache.get(&key(0)).is_some());
        assert!(cache.get(&key(1)).is_none());
        assert_eq!(cache.state().entries.len(), MAX_ENTRIES);
        let state = cache.state();
        assert_eq!(
            state.bytes,
            state
                .entries
                .values()
                .map(|entry| entry.size)
                .sum::<usize>()
        );
    }
    #[test]
    fn read_cache_late_response_does_not_replace_newer_presentation() {
        smol::block_on(async {
            use futures::poll;
            let first = snapshot();
            let second = first.at_operation("bb".into());
            let third = first.at_operation("cc".into());
            let query = Read::ComparisonCandidates;
            let (release, released) = smol::channel::bounded::<()>(1);
            let mut pending = Box::pin(first.read_cached(query.clone(), async {
                released.recv().await?;
                Ok(vec![1u8])
            }));
            assert!(poll!(pending.as_mut()).is_pending());
            second
                .read_cached(query.clone(), async { Ok(vec![2u8]) })
                .await
                .expect("newer response");
            release.send(()).await.expect("release older response");
            pending.await.expect("older response");
            let (source, data) = third
                .previously_cached::<Vec<u8>>(&query)
                .expect("previous presentation");
            assert_eq!(source.operation(), "bb");
            assert_eq!(data, vec![2]);
            assert_eq!(first.cached::<Vec<u8>>(&query), Some(vec![1]));
            assert_eq!(second.cached::<Vec<u8>>(&query), Some(vec![2]));
            assert!(third.cached::<Vec<u8>>(&query).is_none());
        });
    }

    #[test]
    fn read_cache_fixed_commit_contents_survive_unrelated_operations() {
        smol::block_on(async {
            let first = snapshot();
            let second = first.at_operation("bb".into());
            let query = Read::FileContent {
                revision: "a".repeat(40),
                path: "file.txt".into(),
            };
            first
                .read_cached(query.clone(), async { Ok(vec![1u8]) })
                .await
                .expect("fixed content");
            assert_eq!(second.cached::<Vec<u8>>(&query), Some(vec![1]));
            let annotation = Read::Annotate {
                revision: "a".repeat(40),
                path: "file.txt".into(),
            };
            first
                .read_cached(annotation.clone(), async { Ok(Vec::<BlameLine>::new()) })
                .await
                .expect("annotations");
            assert!(
                second.cached::<Vec<BlameLine>>(&annotation).is_none(),
                "unique change-ID prefixes depend on the operation even for a fixed commit"
            );
            first
                .read_cached(
                    Read::FileContent {
                        revision: "a".repeat(40),
                        path: "private.txt".into(),
                    },
                    async { Ok(vec![0xa5u8; 8192]) },
                )
                .await
                .expect("private cached contents");
            assert!(
                format!("{first:?}").len() < 4096,
                "snapshot diagnostics must summarize the cache instead of dumping file contents"
            );
            assert_eq!(
                second
                    .read_cached::<Vec<u8>>(query, async {
                        anyhow::bail!("must reuse fixed commit")
                    })
                    .await
                    .expect("reused"),
                vec![1u8]
            );
        });
    }
}
