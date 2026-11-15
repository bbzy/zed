use super::{KkPanel, RevisionFiles};
use futures::{FutureExt as _, future::Shared};
use gpui::{AppContext as _, Context, Entity, Subscription, Task};
use kk_workbench::{ChangedFile, DiffScope, RepositoryLifetime, Revision, Snapshot};
use language::Buffer;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::Arc,
};
use util::ResultExt as _;

const CONCURRENCY: usize = 2;
const BUFFER_COUNT: usize = 128;
const BUFFER_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, PartialEq, Eq, Hash)]
struct CommitKey {
    root: Arc<Path>,
    operation: Arc<str>,
    commit: Arc<str>,
    lifetime: Option<RepositoryLifetime>,
}

impl CommitKey {
    fn new(snapshot: &Snapshot, commit: &str) -> Self {
        Self {
            root: snapshot.repository().root().into(),
            operation: snapshot.operation().into(),
            commit: commit.into(),
            lifetime: snapshot.repository().lifetime().cloned(),
        }
    }

    fn is_open(&self) -> bool {
        self.lifetime
            .as_ref()
            .is_none_or(|lifetime| lifetime.check().is_ok())
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct Key {
    commit: CommitKey,
    path: Option<Arc<str>>,
}

struct Commit {
    key: CommitKey,
    snapshot: Snapshot,
    revision: Revision,
}

#[derive(Clone)]
struct Job {
    commit: Arc<Commit>,
    file: Option<ChangedFile>,
}

impl Job {
    fn key(&self) -> Key {
        Key {
            commit: self.commit.key.clone(),
            path: self.file.as_ref().map(|file| file.path.as_str().into()),
        }
    }

    async fn read(&self) -> anyhow::Result<bool> {
        let snapshot = &self.commit.snapshot;
        snapshot.repository().check_lifetime()?;
        if let Some(file) = &self.file {
            super::super::kk_diff::DiffContents::prefetch(snapshot, &self.commit.revision, file)
                .await
        } else {
            snapshot
                .revisions(Some(&self.commit.revision.full_commit_id), 2)
                .await?;
            snapshot
                .revisions(Some(&self.commit.revision.revset()), 2)
                .await?;
            snapshot
                .changed_files(&DiffScope::Change(
                    self.commit.revision.full_commit_id.clone(),
                ))
                .await?;
            Ok(false)
        }
    }
}

struct RootQueue<T> {
    root: Arc<Path>,
    items: VecDeque<T>,
}

struct FairQueue<T> {
    tiers: [VecDeque<RootQueue<T>>; 3],
}

impl<T> Default for FairQueue<T> {
    fn default() -> Self {
        Self {
            tiers: std::array::from_fn(|_| VecDeque::new()),
        }
    }
}

impl<T> FairQueue<T> {
    fn push(&mut self, priority: usize, root: Arc<Path>, item: T) {
        let Some(tier) = self.tiers.get_mut(priority) else {
            return;
        };
        if let Some(queue) = tier.iter_mut().find(|queue| queue.root == root) {
            queue.items.push_back(item);
        } else {
            tier.push_back(RootQueue {
                root,
                items: VecDeque::from([item]),
            });
        }
    }

    fn priority(&self) -> Option<usize> {
        self.tiers.iter().position(|tier| !tier.is_empty())
    }

    fn pop(&mut self) -> Option<T> {
        let priority = self.priority()?;
        let tier = self.tiers.get_mut(priority)?;
        let mut queue = tier.pop_front()?;
        let item = queue.items.pop_front();
        if !queue.items.is_empty() {
            tier.push_back(queue);
        }

        item
    }

    fn pop_ready(&mut self, running_priority: Option<usize>) -> Option<T> {
        let priority = self.priority()?;
        if running_priority.is_some_and(|running| running < priority) {
            return None;
        }

        self.pop()
    }

    fn drain(&mut self) -> Vec<T> {
        self.tiers
            .iter_mut()
            .flat_map(|tier| tier.drain(..))
            .flat_map(|queue| queue.items)
            .collect()
    }
}

struct Running {
    task: Shared<Task<()>>,
}

struct PinnedBuffer {
    key: Key,
    buffer: Entity<Buffer>,
    bytes: usize,
    _subscription: Subscription,
}

#[derive(Default)]
pub(super) struct PrefetchState {
    queue: FairQueue<Job>,
    running: HashMap<Key, Running>,
    attempted: HashSet<Key>,
    wanted: HashSet<Key>,
    scopes: HashSet<(CommitKey, bool, bool)>,
    recent: Option<(PathBuf, String)>,
    buffers: VecDeque<PinnedBuffer>,
}

impl PrefetchState {
    fn priority(&self, key: &Key) -> usize {
        if self.recent.as_ref().is_some_and(|(root, commit)| {
            root.as_path() == key.commit.root.as_ref()
                && commit.as_str() == key.commit.commit.as_ref()
        }) {
            0
        } else if key.path.is_some() {
            1
        } else {
            2
        }
    }

    pub(super) fn promote(&mut self, root: &Path, commit: &str) {
        self.recent = Some((root.to_owned(), commit.into()));
        self.reorder();
    }

    fn reorder(&mut self) {
        for job in self.queue.drain() {
            self.queue
                .push(self.priority(&job.key()), job.commit.key.root.clone(), job);
        }
    }

    pub(super) fn wait_for(
        &mut self,
        snapshot: &Snapshot,
        scope: &DiffScope,
        path: &str,
    ) -> Option<Shared<Task<()>>> {
        let DiffScope::Change(commit) = scope else {
            return None;
        };
        self.promote(snapshot.repository().root(), commit);
        let key = Key {
            commit: CommitKey::new(snapshot, commit),
            path: Some(path.into()),
        };
        if let Some(index) = self.buffers.iter().position(|pin| pin.key == key)
            && let Some(pin) = self.buffers.remove(index)
        {
            self.buffers.push_back(pin);
        }

        self.attempted.insert(key.clone());
        let remaining = self.queue.drain();
        for job in remaining.into_iter().filter(|job| job.key() != key) {
            self.queue
                .push(self.priority(&job.key()), job.commit.key.root.clone(), job);
        }

        self.running.get(&key).map(|running| running.task.clone())
    }

    pub(super) fn cancel_closed(&mut self) {
        self.running.retain(|key, _| key.commit.is_open());
        self.buffers.retain(|pin| pin.key.commit.is_open());
        let jobs = self.queue.drain();
        for job in jobs.into_iter().filter(|job| job.commit.key.is_open()) {
            self.queue
                .push(self.priority(&job.key()), job.commit.key.root.clone(), job);
        }
    }

    fn pin(&mut self, key: Key, buffer: Entity<Buffer>, bytes: usize, subscription: Subscription) {
        self.buffers.retain(|pin| pin.key != key);
        let root = key.commit.root.clone();
        self.buffers.push_back(PinnedBuffer {
            key,
            buffer,
            bytes,
            _subscription: subscription,
        });
        loop {
            let (count, bytes) = self
                .buffers
                .iter()
                .filter(|pin| pin.key.commit.root == root)
                .fold((0, 0), |(count, bytes), pin| (count + 1, bytes + pin.bytes));
            if count <= BUFFER_COUNT && bytes <= BUFFER_BYTES {
                break;
            }

            let Some(index) = self
                .buffers
                .iter()
                .position(|pin| pin.key.commit.root == root)
            else {
                break;
            };
            self.buffers.remove(index);
        }
    }
}

impl KkPanel {
    #[expect(
        clippy::mutable_key_type,
        reason = "RepositoryLifetime hashes its stable Arc identity, not its mutable status"
    )]
    pub(super) fn reconcile_prefetch(&mut self, cx: &mut Context<Self>) {
        let scopes = self
            .prefetch_revisions()
            .into_iter()
            .map(|(index, revision)| {
                let repository = &self.repositories[index];
                let snapshot = repository
                    .history_snapshot
                    .as_ref()
                    .unwrap_or(&repository.snapshot)
                    .clone();
                let expanded = repository.expanded.get(&revision.full_commit_id);
                (
                    index,
                    CommitKey::new(&snapshot, &revision.full_commit_id),
                    snapshot,
                    revision,
                    expanded.is_some(),
                    matches!(expanded, Some(RevisionFiles::Loaded(_))),
                )
            })
            .collect::<Vec<_>>();
        let identities = scopes
            .iter()
            .map(|(_, key, _, _, expanded, loaded)| (key.clone(), *expanded, *loaded))
            .collect::<HashSet<_>>();
        if identities != self.prefetch.scopes {
            self.prefetch.scopes = identities;
            let mut wanted = HashSet::new();
            let mut jobs = Vec::new();
            for (index, key, snapshot, revision, _, loaded) in scopes {
                let commit = Arc::new(Commit {
                    key,
                    snapshot,
                    revision,
                });
                let job = Job {
                    commit: commit.clone(),
                    file: None,
                };
                wanted.insert(job.key());
                jobs.push(job);
                if loaded
                    && let Some(RevisionFiles::Loaded(files)) = self.repositories[index]
                        .expanded
                        .get(&commit.revision.full_commit_id)
                {
                    for file in files {
                        let job = Job {
                            commit: commit.clone(),
                            file: Some(file.clone()),
                        };
                        wanted.insert(job.key());
                        jobs.push(job);
                    }
                }
            }

            self.prefetch.attempted.retain(|key| wanted.contains(key));
            self.prefetch.running.retain(|key, running| {
                key.commit.is_open()
                    && (wanted.contains(key)
                        || running.task.strong_count().is_some_and(|count| count > 1))
            });
            self.prefetch
                .buffers
                .retain(|pin| wanted.contains(&pin.key) && pin.key.commit.is_open());
            self.prefetch.queue = FairQueue::default();
            for job in jobs {
                let key = job.key();
                if !self.prefetch.attempted.contains(&key)
                    && !self.prefetch.running.contains_key(&key)
                {
                    self.prefetch
                        .queue
                        .push(self.prefetch.priority(&key), key.commit.root, job);
                }
            }

            self.prefetch.wanted = wanted;
        }

        self.pump_prefetch(cx);
    }

    pub(super) fn pump_prefetch(&mut self, cx: &mut Context<Self>) {
        self.prefetch.running.retain(|key, running| {
            key.commit.is_open()
                && (self.prefetch.wanted.contains(key)
                    || running.task.strong_count().is_some_and(|count| count > 1))
        });
        for pin in &mut self.prefetch.buffers {
            pin.bytes = pin.buffer.read(cx).len();
        }
        self.prefetch.buffers.retain(|pin| {
            pin.key.commit.is_open() && pin.bytes <= kk_workbench::PREFETCH_FILE_BYTES
        });
        let roots = self
            .prefetch
            .buffers
            .iter()
            .map(|pin| pin.key.commit.root.clone())
            .collect::<HashSet<_>>();
        for root in roots {
            loop {
                let (count, bytes) = self
                    .prefetch
                    .buffers
                    .iter()
                    .filter(|pin| pin.key.commit.root == root)
                    .fold((0, 0), |(count, bytes), pin| (count + 1, bytes + pin.bytes));
                if count <= BUFFER_COUNT && bytes <= BUFFER_BYTES {
                    break;
                }

                let Some(index) = self
                    .prefetch
                    .buffers
                    .iter()
                    .position(|pin| pin.key.commit.root == root)
                else {
                    break;
                };
                self.prefetch.buffers.remove(index);
            }
        }

        if self.pending_writes > 0 || self.opening_feedback.is_some() {
            return;
        }

        let pending_priority = self
            .prefetch
            .scopes
            .iter()
            .filter(|(key, expanded, loaded)| {
                *expanded
                    && !*loaded
                    && self.repositories.iter().any(|repository| {
                        repository.snapshot.repository().root() == key.root.as_ref()
                            && matches!(
                                repository.expanded.get(key.commit.as_ref()),
                                Some(RevisionFiles::Loading)
                            )
                    })
            })
            .map(|(commit, _, _)| {
                self.prefetch
                    .priority(&Key {
                        commit: commit.clone(),
                        path: None,
                    })
                    .min(1)
            })
            .min();
        while self.prefetch.running.len() < CONCURRENCY {
            let running_priority = self
                .prefetch
                .running
                .keys()
                .map(|key| self.prefetch.priority(key))
                .chain(pending_priority)
                .min();
            let Some(job) = self.prefetch.queue.pop_ready(running_priority) else {
                break;
            };
            let key = job.key();
            if !key.commit.is_open() {
                continue;
            }

            self.prefetch.attempted.insert(key.clone());
            let task_key = key.clone();
            let task = cx
                .spawn(async move |this, cx| {
                    let reading = job.clone();
                    let result = cx
                        .background_spawn(async move { reading.read().await })
                        .await;
                    let mut buffer = None;
                    if matches!(&result, Ok(true)) && job.commit.revision.is_working_copy {
                        let opening = this
                            .update(cx, |panel, cx| {
                                if !panel.prefetch.wanted.contains(&task_key)
                                    || !task_key.commit.is_open()
                                {
                                    return None;
                                }

                                let file = job.file.as_ref()?;
                                let path = job.commit.key.root.join(&file.path);
                                let workspace = panel.workspace.upgrade()?;
                                let project = workspace.read(cx).project().clone();
                                let project_path = project.read(cx).find_project_path(&path, cx)?;
                                let worktree = project
                                    .read(cx)
                                    .worktree_for_id(project_path.worktree_id, cx)?;
                                let entry = worktree.read(cx).entry_for_path(&project_path.path)?;
                                if !entry.is_file()
                                    || entry.is_fifo
                                    || entry.is_external
                                    || entry.canonical_path.is_some()
                                    || entry.size > kk_workbench::PREFETCH_FILE_BYTES as u64
                                {
                                    return None;
                                }

                                Some(project.update(cx, |project, cx| {
                                    project.open_buffer(project_path, cx)
                                }))
                            })
                            .log_err()
                            .flatten();
                        if let Some(opening) = opening {
                            match opening.await {
                                Ok(opened) => buffer = Some(opened),
                                Err(error) => log::debug!("KK buffer prefetch failed: {error:#}"),
                            }
                        }
                    }

                    if let Err(error) = result {
                        job.commit.snapshot.repository().diagnostics().log(format!(
                            "[PREFETCH] {} {} {:?}: {error:#}",
                            job.commit.key.root.display(),
                            job.commit.key.commit,
                            task_key.path
                        ));
                    }

                    this.update(cx, |panel, cx| {
                        panel.prefetch.running.remove(&task_key);
                        if let Some(buffer) = buffer
                            && panel.prefetch.wanted.contains(&task_key)
                            && task_key.commit.is_open()
                        {
                            let bytes = buffer.read(cx).len();
                            if bytes <= kk_workbench::PREFETCH_FILE_BYTES
                                && !buffer.read(cx).text().contains('\0')
                            {
                                let subscription =
                                    cx.observe(&buffer, |panel, _, cx| panel.pump_prefetch(cx));
                                panel.prefetch.pin(task_key, buffer, bytes, subscription);
                            }
                        }

                        panel.pump_prefetch(cx);
                    })
                    .log_err();
                })
                .shared();
            self.prefetch.running.insert(key, Running { task });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{TreeRowId, remote_tests::run_kk};
    use super::*;
    use gpui::TestAppContext;
    use project::{FakeFs, Project};
    use settings::SettingsStore;
    use workspace::MultiWorkspace;

    async fn finish_prefetch(panel: &Entity<KkPanel>, cx: &mut gpui::VisualTestContext) {
        loop {
            let running = panel.read_with(cx, |panel, _| {
                panel
                    .prefetch
                    .running
                    .values()
                    .map(|running| running.task.clone())
                    .collect::<Vec<_>>()
            });
            if running.is_empty() {
                break;
            }

            futures::future::join_all(running).await;
        }
    }

    #[test]
    fn kk_prefetch_warms_renamed_deleted_and_merged_diff_inputs() -> anyhow::Result<()> {
        futures_lite::future::block_on(async {
            let directory = tempfile::tempdir()?;
            let root = directory.path();
            run_kk(root, &["init", "."]).await?;
            std::fs::write(root.join("old.txt"), "before\n")?;
            std::fs::write(root.join("deleted.txt"), "deleted\n")?;
            std::fs::write(root.join("merged.txt"), "first\nmiddle\nlast\n")?;
            run_kk(root, &["new", "-m", "left"]).await?;
            std::fs::write(root.join("merged.txt"), "left\nmiddle\nlast\n")?;
            let repository = kk_workbench::Repository::open(root, Path::new("kk")).await?;
            let left = repository
                .snapshot(true)
                .await?
                .exact_revision("@")
                .await?
                .full_commit_id;
            run_kk(root, &["new", "@-", "-m", "right"]).await?;
            std::fs::write(root.join("merged.txt"), "first\nmiddle\nright\n")?;
            run_kk(root, &["new", "@", &left, "-m", "merge"]).await?;
            std::fs::rename(root.join("old.txt"), root.join("renamed.txt"))?;
            std::fs::remove_file(root.join("deleted.txt"))?;
            std::fs::write(root.join("merged.txt"), "left\nchanged\nright\n")?;
            let repository = kk_workbench::Repository::open(root, Path::new("kk")).await?;
            let snapshot = repository.snapshot(true).await?;
            let revision = snapshot.exact_revision("@").await?;
            let files = snapshot
                .changed_files(&DiffScope::Change(revision.full_commit_id.clone()))
                .await?;
            assert_eq!(files.len(), 3);
            for file in &files {
                super::super::super::kk_diff::DiffContents::prefetch(&snapshot, &revision, file)
                    .await?;
                let cached = snapshot
                    .cached::<(kk_workbench::FileInformation, Option<Vec<u8>>)>(
                        &kk_workbench::remote::Read::MergedParentFile {
                            revision: revision.full_commit_id.clone(),
                            file: file.clone(),
                        },
                    )
                    .expect("merged base was prefetched");
                assert_eq!(
                    cached.1,
                    snapshot
                        .merged_parent_file(&revision.full_commit_id, file)
                        .await?
                        .1
                );
                assert!(cached.0.unavailable.is_none());
            }
            assert!(
                files
                    .iter()
                    .any(|file| file.renamed_from.as_deref() == Some("old.txt"))
            );
            Ok(())
        })
    }

    #[gpui::test(iterations = 3)]
    async fn kk_prefetch_expansion_warms_metadata_and_native_buffers_without_selection_changes(
        cx: &mut TestAppContext,
    ) {
        cx.background_executor.allow_parking();
        let (directory, revision) = cx
            .background_spawn(async {
                let directory = tempfile::tempdir()?;
                run_kk(directory.path(), &["init", "."]).await?;
                std::fs::write(directory.path().join("file.txt"), "base\n")?;
                run_kk(directory.path(), &["new", "-m", "work"]).await?;
                std::fs::write(directory.path().join("file.txt"), "after\n")?;
                let repository =
                    kk_workbench::Repository::open(directory.path(), Path::new("kk")).await?;
                let revision = repository.snapshot(true).await?.exact_revision("@").await?;
                anyhow::Ok((directory, revision))
            })
            .await
            .expect("repository");
        let root = std::fs::canonicalize(directory.path()).expect("root");
        cx.update(|cx| {
            let settings = SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            crate::init(cx);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(&root, serde_json::json!({"file.txt": "after\n"}))
            .await;
        cx.update(|cx| <dyn project::Fs>::set_global(fs.clone(), cx));
        let project = Project::test(fs.clone(), [root.as_path()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |view, _| view.workspace().clone());
        let panel = cx
            .update(|window, cx| KkPanel::load(workspace.downgrade(), window.to_async(cx)))
            .await
            .expect("panel");
        panel
            .update(cx, |panel, cx| panel.refresh_for_test(cx))
            .await;
        cx.condition(&panel, |panel, _| !panel.loading).await;
        panel.update(cx, |panel, cx| {
            panel
                .tree_selection
                .expanded
                .insert(TreeRowId::History(root.clone()), true);
            panel.update_tree(cx);
        });
        let query = kk_workbench::remote::Read::ChangedFiles {
            scope: DiffScope::Change(revision.full_commit_id.clone()),
        };
        finish_prefetch(&panel, cx).await;
        panel.read_with(cx, |panel, _| {
            assert!(
                panel.repositories[0]
                    .snapshot
                    .cached::<Vec<ChangedFile>>(&query)
                    .is_some()
            );
            assert!(
                !panel.repositories[0]
                    .expanded
                    .contains_key(&revision.full_commit_id)
            );
            assert!(panel.opening_feedback.is_none());
        });
        panel.update(cx, |panel, cx| {
            panel.load_files(0, revision.clone(), false, cx)
        });
        let selection = panel.read_with(cx, |panel, _| panel.tree_selection.selected.clone());
        finish_prefetch(&panel, cx).await;
        let buffer = panel.read_with(cx, |panel, _| {
            assert_eq!(panel.tree_selection.selected, selection);
            assert!(panel.opening_feedback.is_none());
            panel
                .prefetch
                .buffers
                .front()
                .expect("native buffer")
                .buffer
                .clone()
        });
        assert_eq!(buffer.read_with(cx, |buffer, _| buffer.text()), "after\n");
        fs.insert_file(root.join("file.txt"), b"disk update\n".to_vec())
            .await;
        cx.condition(&buffer, |buffer, _| buffer.text() == "disk update\n")
            .await;
        buffer.update(cx, |buffer, cx| buffer.edit([(0..0, "draft\n")], None, cx));
        panel.update(cx, |panel, cx| panel.reconcile_prefetch(cx));
        assert_eq!(
            buffer.read_with(cx, |buffer, _| buffer.text()),
            "draft\ndisk update\n"
        );
        let opened = panel
            .update_in(cx, |panel, window, cx| {
                let snapshot = panel.repositories[0].snapshot.clone();
                let Some(RevisionFiles::Loaded(files)) =
                    panel.repositories[0].expanded.get(&revision.full_commit_id)
                else {
                    panic!("prefetched files");
                };
                let file = files.first().expect("changed file").clone();
                panel.open_diff_task(
                    snapshot,
                    super::super::super::kk_diff::DiffTarget::change(&revision),
                    file,
                    true,
                    super::super::super::kk_diff::DiffOpenMode::Hidden,
                    window,
                    cx,
                )
            })
            .await
            .expect("open diff")
            .expect("current opening");
        cx.update(|_, cx| {
            use workspace::item::ItemHandle as _;
            let editor = opened
                .act_as_type(std::any::TypeId::of::<editor::Editor>(), cx)
                .expect("native editor")
                .downcast::<editor::Editor>()
                .expect("editor type");
            assert!(
                editor
                    .read(cx)
                    .buffer()
                    .read(cx)
                    .all_buffers()
                    .contains(&buffer)
            );
            assert_eq!(buffer.read(cx).text(), "draft\ndisk update\n");
        });
        panel.update(cx, |panel, cx| {
            panel.load_files(0, revision.clone(), false, cx)
        });
        panel.read_with(cx, |panel, _| assert!(panel.prefetch.buffers.is_empty()));
        assert!(buffer.read_with(cx, |buffer, _| buffer.is_dirty()));
        panel.update(cx, |panel, cx| {
            panel
                .tree_selection
                .expanded
                .insert(TreeRowId::History(root.clone()), false);
            panel.update_tree(cx);
            assert!(panel.prefetch.running.is_empty());
            assert!(panel.prefetch.queue.pop().is_none());

            let snapshot = panel.repositories[0].snapshot.clone();
            let key = Key {
                commit: CommitKey::new(&snapshot, &revision.full_commit_id),
                path: Some("file.txt".into()),
            };
            panel.prefetch.running.insert(
                key.clone(),
                Running {
                    task: Task::ready(()).shared(),
                },
            );
            let foreground = panel
                .prefetch
                .wait_for(
                    &snapshot,
                    &DiffScope::Change(revision.full_commit_id.clone()),
                    "file.txt",
                )
                .expect("join the running target");
            panel.pump_prefetch(cx);
            assert!(
                panel.prefetch.running.contains_key(&key),
                "folding preserves the foreground waiter"
            );
            drop(foreground);
            panel.pump_prefetch(cx);
            assert!(
                panel.prefetch.running.is_empty(),
                "cancel when the foreground waiter leaves"
            );

            for index in 0..BUFFER_COUNT {
                let mut key = key.clone();
                key.path = Some(index.to_string().into());
                let subscription = cx.observe(&buffer, |panel, _, cx| panel.pump_prefetch(cx));
                panel.prefetch.pin(key, buffer.clone(), 1, subscription);
            }
            drop(panel.prefetch.wait_for(
                &snapshot,
                &DiffScope::Change(revision.full_commit_id.clone()),
                "0",
            ));
            let mut newest = key;
            newest.path = Some("newest".into());
            let subscription = cx.observe(&buffer, |panel, _, cx| panel.pump_prefetch(cx));
            panel.prefetch.pin(newest, buffer.clone(), 1, subscription);
            assert_eq!(panel.prefetch.buffers.len(), BUFFER_COUNT);
            assert!(
                panel
                    .prefetch
                    .buffers
                    .iter()
                    .any(|pin| pin.key.path.as_deref() == Some("0"))
            );
            assert!(
                !panel
                    .prefetch
                    .buffers
                    .iter()
                    .any(|pin| pin.key.path.as_deref() == Some("1"))
            );
        });
    }

    #[test]
    fn kk_prefetch_interaction_overtakes_backlog_without_preempting_running_jobs() {
        let mut queue = FairQueue::default();
        let root: Arc<Path> = Path::new("repo").into();
        for commit in 0..8 {
            queue.push(2, root.clone(), commit);
        }
        assert_eq!(queue.pop_ready(None), Some(0));
        assert_eq!(queue.pop_ready(Some(2)), Some(1));
        queue.push(0, root.clone(), 100);
        assert_eq!(queue.pop_ready(Some(2)), Some(100));
        // The next slot waits for the selected commit to discover its files.
        assert_eq!(queue.pop_ready(Some(0)), None);
        queue.push(0, root.clone(), 101);
        queue.push(0, root, 102);
        assert_eq!(queue.pop_ready(Some(0)), Some(101));
        assert_eq!(queue.pop_ready(Some(0)), Some(102));
        assert_eq!(queue.pop_ready(Some(0)), None);
        assert_eq!(queue.pop_ready(None), Some(2));
    }

    #[test]
    fn kk_prefetch_is_fair_between_repositories_within_each_priority() {
        let mut queue = FairQueue::default();
        for value in [1, 2, 3] {
            queue.push(1, Path::new("first").into(), value);
        }
        for value in [4, 5] {
            queue.push(1, Path::new("second").into(), value);
        }
        queue.push(0, Path::new("first").into(), 0);
        assert_eq!(
            (0..6).filter_map(|_| queue.pop()).collect::<Vec<_>>(),
            [0, 1, 4, 2, 5, 3]
        );
        assert_eq!(queue.pop(), None);
    }
}
