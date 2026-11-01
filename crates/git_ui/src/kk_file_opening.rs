use super::super::kk_classification::{ClassificationFailure, RepositoryQueue, SparseCheckoutJob};
use super::KkPanel;
use futures::{
    FutureExt as _,
    future::{BoxFuture, Either, Shared, select},
};
use gpui::{AppContext as _, Context, Task, Window};
use kk_workbench::{RepositoryLifetime, Snapshot};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
};
use util::ResultExt as _;

type CheckoutResult = Result<Option<Snapshot>, Arc<str>>;
type DiffOpeningResult = Result<Option<gpui::Entity<super::super::kk_diff::KkDiff>>, Arc<str>>;

struct PendingDiffOpening {
    key: DiffOpeningKey,
    opening: OpeningToken,
    task: Shared<Task<DiffOpeningResult>>,
}

#[derive(PartialEq, Eq)]
struct DiffOpeningKey {
    root: PathBuf,
    operation: String,
    request: String,
    lifetime: Option<RepositoryLifetime>,
}

#[derive(Clone)]
pub(in super::super) struct OpeningToken(Arc<OpeningRequest>);

struct OpeningRequest {
    active: AtomicBool,
    cancellation: Mutex<Option<futures::channel::oneshot::Sender<()>>>,
    cancelled: Shared<BoxFuture<'static, ()>>,
    lifetime: Option<RepositoryLifetime>,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct CheckoutKey {
    root: PathBuf,
    operation: String,
    paths: Vec<String>,
    lifetime: Option<RepositoryLifetime>,
}

struct PendingCheckout {
    interests: Arc<Mutex<Vec<Weak<OpeningRequest>>>>,
    task: Shared<Task<CheckoutResult>>,
}

#[derive(Default)]
pub(in super::super) struct FileOpeningState {
    latest: Weak<OpeningRequest>,
    diff: Option<PendingDiffOpening>,
    pending: HashMap<CheckoutKey, PendingCheckout>,
}

impl FileOpeningState {
    pub fn begin(&mut self, snapshot: &Snapshot) -> OpeningToken {
        self.diff = None;
        if let Some(previous) = self.latest.upgrade() {
            previous.cancel();
        }

        let (cancellation, cancelled) = futures::channel::oneshot::channel();
        let request = Arc::new(OpeningRequest {
            active: AtomicBool::new(true),
            cancellation: Mutex::new(Some(cancellation)),
            cancelled: cancelled.map(|_| ()).boxed().shared(),
            lifetime: snapshot.repository().lifetime().cloned(),
        });
        self.latest = Arc::downgrade(&request);
        OpeningToken(request)
    }

    pub fn cancel_closed(&self) {
        if let Some(latest) = self.latest.upgrade()
            && !latest.is_current()
        {
            latest.cancel();
        }
    }
}

impl OpeningRequest {
    fn is_current(&self) -> bool {
        self.active.load(Ordering::Acquire)
            && self
                .lifetime
                .as_ref()
                .is_none_or(|lifetime| lifetime.check().is_ok())
    }

    fn cancel(&self) {
        self.active.store(false, Ordering::Release);
        self.cancellation
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
    }
}

impl OpeningToken {
    pub fn is_current(&self) -> bool {
        self.0.is_current()
    }

    pub async fn wait<T: 'static>(
        &self,
        task: Task<anyhow::Result<T>>,
    ) -> anyhow::Result<Option<T>> {
        if !self.is_current() {
            return Ok(None);
        }

        match select(std::pin::pin!(task), self.0.cancelled.clone()).await {
            Either::Left((result, _)) if self.is_current() => result.map(Some),
            _ => Ok(None),
        }
    }
}

impl KkPanel {
    pub(super) fn track_opening<T: 'static>(
        &mut self,
        label: String,
        rows: Option<std::collections::HashSet<super::tree::TreeRowId>>,
        task: Task<anyhow::Result<T>>,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<T>> {
        self.opening_sequence += 1;
        let sequence = self.opening_sequence;
        self.opening_feedback = Some((
            sequence,
            label,
            rows.unwrap_or_else(|| self.tree_selection.selected.clone()),
        ));
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                if this
                    .opening_feedback
                    .as_ref()
                    .is_some_and(|(current, _, _)| *current == sequence)
                {
                    this.opening_feedback = None;
                    cx.notify();
                }
            })
            .log_err();
            result
        })
    }

    pub(in super::super) fn open_all_changes(
        &mut self,
        index: usize,
        revision: Option<kk_workbench::Revision>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(repository) = self.repositories.get(index) else {
            return;
        };
        let (mut target, working_copy) = if let Some(revision) = revision {
            (
                super::super::kk_diff::DiffTarget::change(&revision),
                revision.is_working_copy,
            )
        } else if let Some(comparison) = &repository.comparison {
            (comparison.diff_target(), true)
        } else {
            return;
        };
        if let Some(comparison) = &repository.comparison
            && match &target.scope {
                kk_workbench::DiffScope::Change(commit) => comparison.sources.contains(commit),
                kk_workbench::DiffScope::Comparison { .. } => true,
            }
        {
            target.comparison = Some(comparison.intent.clone());
        }
        let snapshot = match self.bind_snapshot(repository.snapshot.clone(), cx) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                Self::show_error(self, error.to_string(), cx);
                return;
            }
        };
        let opening = self.file_opening.begin(&snapshot);
        self.error = None;
        cx.notify();
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = async {
                let (snapshot, target, files) = cx
                    .background_spawn(async move {
                        let files = snapshot.changed_files(&target.scope).await?;
                        anyhow::Ok((snapshot, target, files))
                    })
                    .await?;
                if !opening.is_current() {
                    return Ok(());
                }
                let paths = files
                    .iter()
                    .filter(|file| {
                        working_copy && file.change_type != kk_workbench::ChangeKind::Deleted
                    })
                    .map(|file| file.path.clone())
                    .collect();
                let preparation = this.update_in(cx, |this, window, cx| {
                    this.prepare_open(snapshot, paths, &opening, window, cx)
                })?;
                let Some(snapshot) = preparation
                    .await
                    .map_err(|error| anyhow::anyhow!(error.to_string()))?
                else {
                    return Ok(());
                };
                if !opening.is_current() {
                    return Ok(());
                }
                let open = this.update_in(cx, |this, window, cx| {
                    super::super::kk_multi_diff::KkMultiDiff::open(
                        snapshot,
                        target,
                        files,
                        working_copy,
                        this.workspace.clone(),
                        super::super::kk_diff::DiffOpenMode::Open,
                        window,
                        cx,
                    )
                })?;
                opening.wait(open).await?;
                anyhow::Ok(())
            }
            .await;
            if let Err(error) = result
                && opening.is_current()
            {
                this.update(cx, |this, cx| {
                    Self::show_error(this, format!("{error:#}"), cx)
                })
                .log_err();
            }
        });
        let task = cx.spawn(async move |_, _| {
            task.await;
            anyhow::Ok(())
        });
        let task = self.track_opening("Opening changes…".into(), None, task, cx);
        self.load_tasks.insert(
            "open-diff".into(),
            cx.spawn(async move |_, _| {
                task.await.log_err();
            }),
        );
    }

    pub(in super::super) fn open_diff_task(
        &mut self,
        snapshot: Snapshot,
        target: super::super::kk_diff::DiffTarget,
        file: kk_workbench::ChangedFile,
        working_copy: bool,
        mode: super::super::kk_diff::DiffOpenMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<Option<gpui::Entity<super::super::kk_diff::KkDiff>>>> {
        let snapshot = match self.bind_snapshot(snapshot, cx) {
            Ok(snapshot) => snapshot,
            Err(error) => return Task::ready(Err(error)),
        };
        if let Some(existing) = super::super::kk_diff::KkDiff::reopen(
            snapshot.clone(),
            &target,
            &file,
            working_copy,
            self.workspace.clone(),
            mode,
            window,
            cx,
        ) {
            self.file_opening.begin(&snapshot);
            return Task::ready(Ok(Some(existing)));
        }
        let identity = match serde_json::to_string(&(&target, &file, working_copy, mode as u8)) {
            Ok(identity) => identity,
            Err(error) => return Task::ready(Err(error.into())),
        };
        let key = DiffOpeningKey {
            root: snapshot.repository().root().to_owned(),
            operation: snapshot.operation().to_owned(),
            request: identity,
            lifetime: snapshot.repository().lifetime().cloned(),
        };
        if let Some(pending) = &self.file_opening.diff
            && pending.key == key
            && pending.opening.is_current()
        {
            let task = pending.task.clone();
            return cx.spawn(async move |_, _| {
                task.await
                    .map_err(|error| anyhow::anyhow!(error.to_string()))
            });
        }

        let opening = self.file_opening.begin(&snapshot);
        let task = self.open_diff_with_token(
            snapshot,
            target,
            file,
            working_copy,
            mode,
            opening.clone(),
            window,
            cx,
        );
        let completed = opening.clone();
        let task = cx
            .spawn(async move |this, cx| {
                let result = task.await.map_err(|error| Arc::from(format!("{error:#}")));
                this.update(cx, |this, _| {
                    if this
                        .file_opening
                        .diff
                        .as_ref()
                        .is_some_and(|pending| Arc::ptr_eq(&pending.opening.0, &completed.0))
                    {
                        this.file_opening.diff = None;
                    }
                })
                .log_err();
                result
            })
            .shared();
        self.file_opening.diff = Some(PendingDiffOpening {
            key,
            opening,
            task: task.clone(),
        });
        cx.spawn(async move |_, _| {
            task.await
                .map_err(|error| anyhow::anyhow!(error.to_string()))
        })
    }

    pub(in super::super) fn open_search_diff_task(
        &mut self,
        snapshot: Snapshot,
        target: super::super::kk_diff::DiffTarget,
        path: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<Option<gpui::Entity<super::super::kk_diff::KkDiff>>>> {
        let snapshot = match self.bind_snapshot(snapshot, cx) {
            Ok(snapshot) => snapshot,
            Err(error) => return Task::ready(Err(error)),
        };
        let opening = self.file_opening.begin(&snapshot);
        cx.spawn_in(window, async move |this, cx| {
            let resolved = cx.background_spawn(async move {
                let _priority = snapshot.repository().begin_diff_opening().await?;
                let snapshot = snapshot.repository().snapshot(false).await?;
                let (target, working_copy) =
                    if matches!(target.scope, kk_workbench::DiffScope::Comparison { .. })
                        && let Some(intent) = &target.comparison
                    {
                        (
                            super::super::kk_comparison::Comparison::load(
                                &snapshot,
                                intent.clone(),
                            )
                            .await?
                            .diff_target(),
                            true,
                        )
                    } else {
                        target.resolve(&snapshot).await?
                    };
                let file = snapshot
                    .changed_files(&target.scope)
                    .await?
                    .into_iter()
                    .find(|file| file.path == path)
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "The current change no longer contains {path}. Run the search again."
                        )
                    })?;
                anyhow::Ok((snapshot, target, file, working_copy))
            });
            let Some((snapshot, target, file, working_copy)) = opening.wait(resolved).await? else {
                return Ok(None);
            };
            this.update_in(cx, |this, window, cx| {
                this.open_diff_with_token(
                    snapshot,
                    target,
                    file,
                    working_copy,
                    super::super::kk_diff::DiffOpenMode::Open,
                    opening,
                    window,
                    cx,
                )
            })?
            .await
        })
    }

    fn open_diff_with_token(
        &mut self,
        snapshot: Snapshot,
        target: super::super::kk_diff::DiffTarget,
        file: kk_workbench::ChangedFile,
        working_copy: bool,
        mode: super::super::kk_diff::DiffOpenMode,
        opening: OpeningToken,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<Option<gpui::Entity<super::super::kk_diff::KkDiff>>>> {
        let paths = if working_copy && file.change_type != kk_workbench::ChangeKind::Deleted {
            vec![file.path.clone()]
        } else {
            Vec::new()
        };
        let preparation = self.prepare_open(snapshot, paths, &opening, window, cx);
        cx.spawn_in(window, async move |this, cx| {
            let preparation = preparation.await;
            if !opening.is_current() {
                return Ok(None);
            }

            let Some(snapshot) = preparation.map_err(|error| anyhow::anyhow!(error.to_string()))?
            else {
                return Ok(None);
            };

            let open = this.update_in(cx, |this, window, cx| {
                super::super::kk_diff::KkDiff::open(
                    snapshot,
                    target,
                    file,
                    working_copy,
                    this.workspace.clone(),
                    mode,
                    window,
                    cx,
                )
            })?;
            opening.wait(open).await
        })
    }

    pub(in super::super) fn prepare_open(
        &mut self,
        snapshot: Snapshot,
        mut paths: Vec<String>,
        opening: &OpeningToken,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Shared<Task<CheckoutResult>> {
        paths.sort();
        paths.dedup();
        if paths.is_empty() || !opening.is_current() {
            return Task::ready(Ok(opening.is_current().then_some(snapshot))).shared();
        }

        let key = CheckoutKey {
            root: snapshot.repository().root().to_owned(),
            operation: snapshot.operation().to_owned(),
            paths,
            lifetime: snapshot.repository().lifetime().cloned(),
        };
        if let Some(pending) = self.file_opening.pending.get(&key) {
            pending
                .interests
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(Arc::downgrade(&opening.0));
            return pending.task.clone();
        }

        let interests = Arc::new(Mutex::new(vec![Arc::downgrade(&opening.0)]));
        let current: Arc<dyn Fn() -> bool + Send + Sync> = {
            let interests = interests.clone();
            Arc::new(move || {
                interests
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .iter()
                    .any(|interest| {
                        interest
                            .upgrade()
                            .is_some_and(|interest| interest.is_current())
                    })
            })
        };
        let task_key = key.clone();
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = async {
                if !current() {
                    return anyhow::Ok(None);
                }

                let excluded = cx.background_spawn({
                    let snapshot = snapshot.clone();
                    let paths = task_key.paths.clone();
                    async move { snapshot.repository().excluded_sparse_files(&paths).await }
                }).await?;
                if !current() {
                    return Ok(None);
                }

                if excluded.is_empty() {
                    return Ok(Some(snapshot));
                }

                let prompt = this.update_in(cx, |_, window, cx| {
                    let message = if let [path] = excluded.as_slice() {
                        format!("“{path}” is outside this workspace’s sparse checkout. Add it and open the file?")
                    } else {
                        format!("{} selected files are outside this workspace’s sparse checkout. Add them and open the files?", excluded.len())
                    };
                    let prompt = window.prompt(gpui::PromptLevel::Info, &message, (excluded.len() > 1).then(|| excluded.join("\n")).as_deref(), &["Yes", "No"], cx);
                    cx.notify();
                    prompt
                })?;
                if prompt.await? != 0 || !current() {
                    return Ok(None);
                }

                let completed = this.update(cx, |panel, cx| panel.queue_sparse_checkout(snapshot, excluded, cx, current.clone()))?.await.map_err(|failure| anyhow::anyhow!(failure.message))?;
                Ok(current().then_some(completed))
            }.await;
            this.update(cx, |panel, _| { panel.file_opening.pending.remove(&task_key); }).log_err();
            result.map_err(|error| Arc::<str>::from(format!("{error:#}")))
        }).shared();
        self.file_opening.pending.insert(
            key,
            PendingCheckout {
                interests,
                task: task.clone(),
            },
        );
        task
    }

    fn queue_sparse_checkout(
        &mut self,
        snapshot: Snapshot,
        paths: Vec<String>,
        cx: &mut Context<Self>,
        current: Arc<dyn Fn() -> bool + Send + Sync>,
    ) -> Task<Result<Snapshot, ClassificationFailure>> {
        let snapshot = match self.bind_write_snapshot(snapshot, cx) {
            Ok(snapshot) => snapshot,
            Err(error) => return Task::ready(Err(error)),
        };
        let root = snapshot.repository().root().to_owned();
        if self.uncertain_operations.contains_key(&root) {
            return Task::ready(Err(ClassificationFailure { message: "Refresh KK Workbench to confirm the previous operation before expanding the sparse checkout".into(), uncertain: false }));
        }

        let (completion, completed) = futures::channel::oneshot::channel();
        self.pending_writes += 1;
        self.pending_status = "Adding files to sparse checkout…";
        let start = self
            .write_queues
            .entry(root.clone())
            .or_insert_with(|| RepositoryQueue::new(cx.background_executor().clone()))
            .enqueue_sparse(SparseCheckoutJob {
                snapshot,
                paths,
                current,
                completion,
            });
        if start {
            self.run_write_queue(root, cx);
        }

        cx.notify();
        cx.spawn(async move |_, _| {
            completed.await.unwrap_or_else(|_| {
                Err(ClassificationFailure {
                    message: "The sparse checkout operation was interrupted".into(),
                    uncertain: false,
                })
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kk_panel::remote_tests::run_kk;
    use gpui::TestAppContext;
    use project::{FakeFs, Project};
    use settings::SettingsStore;
    use std::path::Path;
    use workspace::MultiWorkspace;

    #[gpui::test(iterations = 3)]
    async fn sparse_open_prompts_once_and_respects_replacement_and_repository_close(
        cx: &mut TestAppContext,
    ) {
        cx.background_executor.allow_parking();
        let (directory, snapshot, latest, history) = cx
            .background_spawn(async {
                futures_lite::future::block_on(async {
                    let directory = tempfile::tempdir()?;
                    run_kk(directory.path(), &["init", "."]).await?;
                    for path in ["visible.txt", "first.txt", "second.txt", "closed.txt"] {
                        std::fs::write(directory.path().join(path), format!("{path}\n"))?;
                    }

                    run_kk(directory.path(), &["new"]).await?;
                    run_kk(
                        directory.path(),
                        &["sparse", "set", "--clear", "--add=visible.txt"],
                    )
                    .await?;
                    let repository =
                        kk_workbench::Repository::open(directory.path(), Path::new("kk")).await?;
                    let snapshot = repository.snapshot(false).await?;
                    let history = snapshot
                        .history(kk_workbench::HistoryMode::Lineage, 50)
                        .await?;
                    run_kk(directory.path(), &["describe", "-m", "updated snapshot"]).await?;
                    let latest = repository.snapshot(true).await?;
                    anyhow::Ok((directory, snapshot, latest, history))
                })
            })
            .await
            .expect("prepare sparse repository");
        let root = snapshot.repository().root().to_owned();
        cx.update(|cx| {
            let settings = SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(&root, serde_json::json!({"visible.txt": "visible.txt\n"}))
            .await;
        let project = Project::test(fs, [root.as_path()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = multi_workspace.read_with(cx, |view, _| view.workspace().clone());
        let panel = cx
            .update(|window, cx| KkPanel::load(workspace.downgrade(), window.to_async(cx)))
            .await
            .expect("panel");
        let snapshot = panel.update(cx, |panel, cx| {
            panel.repositories = vec![super::super::RepositoryView {
                stale: false,
                snapshot: snapshot.clone(),
                history,
                expanded: HashMap::new(),
                comparison: None,
                located: None,
            }];
            panel.bind_snapshot(snapshot, cx).expect("bind repository")
        });
        let (first_sender, first_receiver) = futures::channel::oneshot::channel::<()>();
        let (second_sender, second_receiver) = futures::channel::oneshot::channel::<()>();
        let (first_feedback, second_feedback) = panel.update(cx, |panel, cx| {
            let first = cx.spawn(async move |_, _| {
                first_receiver.await?;
                anyhow::Ok(())
            });
            let first = panel.track_opening("first".into(), None, first, cx);
            assert_eq!(panel.opening_feedback.as_ref().unwrap().1, "first");
            let second = cx.spawn(async move |_, _| {
                second_receiver.await?;
                anyhow::bail!("opening failed")
            });
            let second = panel.track_opening::<()>("second".into(), None, second, cx);
            (first, second)
        });
        first_sender.send(()).expect("complete first opening");
        first_feedback.await.expect("first opening succeeds");
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.opening_feedback.as_ref().unwrap().1, "second");
        });
        second_sender.send(()).expect("complete second opening");
        assert!(second_feedback.await.is_err());
        panel.read_with(cx, |panel, _| assert!(panel.opening_feedback.is_none()));
        let latest = latest.with_lifetime(snapshot.repository().lifetime().cloned());
        let (_first, initial_preflight, _second, latest_preflight) =
            panel.update_in(cx, |panel, window, cx| {
                let first = panel.file_opening.begin(&snapshot);
                let initial = panel.prepare_open(
                    snapshot.clone(),
                    vec!["visible.txt".into()],
                    &first,
                    window,
                    cx,
                );
                let second = panel.file_opening.begin(&latest);
                let latest = panel.prepare_open(
                    latest.clone(),
                    vec!["visible.txt".into()],
                    &second,
                    window,
                    cx,
                );
                (first, initial, second, latest)
            });
        initial_preflight.await.expect("superseded preflight");
        assert_eq!(
            latest_preflight
                .await
                .expect("latest preflight")
                .expect("available file")
                .operation(),
            latest.operation(),
            "same-path navigation must retain its own operation snapshot"
        );

        let (release, released) = futures::channel::oneshot::channel();
        let failed_preflight = cx
            .spawn(async move |_| {
                released.await.expect("release failed preflight");
                Err(Arc::<str>::from("old preflight failed"))
            })
            .shared();
        let cancelled = panel.update_in(cx, |panel, window, cx| {
            panel.file_opening.pending.insert(
                CheckoutKey {
                    root: root.clone(),
                    operation: snapshot.operation().to_owned(),
                    paths: vec!["visible.txt".into()],
                    lifetime: snapshot.repository().lifetime().cloned(),
                },
                PendingCheckout {
                    interests: Arc::new(Mutex::new(Vec::new())),
                    task: failed_preflight,
                },
            );
            let task = panel.open_diff_task(
                snapshot.clone(),
                super::super::super::kk_diff::DiffTarget::comparison(
                    "root()".into(),
                    "root()".into(),
                    "base".into(),
                    None,
                ),
                kk_workbench::ChangedFile {
                    path: "visible.txt".into(),
                    change_type: kk_workbench::ChangeKind::Modified,
                    renamed_from: None,
                    has_conflict: false,
                },
                true,
                super::super::super::kk_diff::DiffOpenMode::Preview,
                window,
                cx,
            );
            let opening = panel.file_opening.latest.clone();
            let duplicate = panel.open_diff_task(
                snapshot.clone(),
                super::super::super::kk_diff::DiffTarget::comparison(
                    "root()".into(),
                    "root()".into(),
                    "base".into(),
                    None,
                ),
                kk_workbench::ChangedFile {
                    path: "visible.txt".into(),
                    change_type: kk_workbench::ChangeKind::Modified,
                    renamed_from: None,
                    has_conflict: false,
                },
                true,
                super::super::super::kk_diff::DiffOpenMode::Preview,
                window,
                cx,
            );
            assert!(Weak::ptr_eq(&opening, &panel.file_opening.latest));
            drop(task);
            panel.file_opening.begin(&snapshot);
            duplicate
        });
        release.send(()).expect("release stale failure");
        assert!(
            cancelled
                .await
                .expect("stale errors must not escape")
                .is_none()
        );
        panel.update(cx, |panel, _| panel.file_opening.pending.clear());
        let (first, duplicate) = panel.update_in(cx, |panel, window, cx| {
            let mut open = || {
                panel.open_diff_task(
                    snapshot.clone(),
                    super::super::super::kk_diff::DiffTarget::comparison(
                        "root()".into(),
                        "root()".into(),
                        "base".into(),
                        None,
                    ),
                    kk_workbench::ChangedFile {
                        path: "visible.txt".into(),
                        change_type: kk_workbench::ChangeKind::Added,
                        renamed_from: None,
                        has_conflict: false,
                    },
                    true,
                    super::super::super::kk_diff::DiffOpenMode::Open,
                    window,
                    cx,
                )
            };
            let first = open();
            let duplicate = open();
            (first, duplicate)
        });
        let first = first.await.expect("first open").expect("current open");
        let duplicate = duplicate
            .await
            .expect("joined open")
            .expect("current duplicate");
        assert_eq!(first, duplicate, "duplicate requests share the loaded view");
        let reopened = panel.update_in(cx, |panel, window, cx| {
            panel
                .open_diff_task(
                    snapshot.clone(),
                    super::super::super::kk_diff::DiffTarget::comparison(
                        "root()".into(),
                        "root()".into(),
                        "base".into(),
                        None,
                    ),
                    kk_workbench::ChangedFile {
                        path: "visible.txt".into(),
                        change_type: kk_workbench::ChangeKind::Added,
                        renamed_from: None,
                        has_conflict: false,
                    },
                    true,
                    super::super::super::kk_diff::DiffOpenMode::Open,
                    window,
                    cx,
                )
                .now_or_never()
                .expect("reopening bypasses checkout reads")
                .expect("reopen existing diff")
                .expect("existing view")
        });
        assert_eq!(reopened, first);
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(
                workspace
                    .items_of_type::<super::super::super::kk_diff::KkDiff>(cx)
                    .count(),
                1
            );
        });

        let cancelled = panel.update_in(cx, |panel, window, cx| {
            let repository = panel.repositories.first_mut().expect("test repository");
            repository.comparison = Some(super::super::Comparison::pending(
                super::super::ComparisonIntent::decode(
                    r#"{"version":1,"target_revset":"cancelled-aggregate-missing-revision","target_label":"removed target","range_start_change_id":null}"#,
                )
                .expect("comparison intent"),
            ));
            panel.open_all_changes(0, None, window, cx);
            panel.file_opening.begin(&snapshot);
            panel.error = Some("newer navigation".into());
            panel
                .load_tasks
                .remove("open-diff")
                .expect("aggregate open")
        });
        cancelled.await;
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.error.as_deref(), Some("newer navigation"));
        });
        panel.update(cx, |panel, _| {
            panel
                .repositories
                .first_mut()
                .expect("test repository")
                .comparison = None;
        });
        let prompt_context = cx.cx.clone();
        let begin = |panel: &mut KkPanel, window: &mut Window, cx: &mut Context<KkPanel>| {
            let opening = panel.file_opening.begin(&snapshot);
            let task = panel.prepare_open(
                snapshot.clone(),
                vec!["second.txt".into(), "first.txt".into(), "first.txt".into()],
                &opening,
                window,
                cx,
            );
            (opening, task)
        };
        let (_declined, declined) = panel.update_in(cx, begin);
        cx.condition(&panel, |_, _| prompt_context.has_pending_prompt())
            .await;
        assert_eq!(
            cx.pending_prompt().expect("sparse confirmation").1,
            "first.txt\nsecond.txt"
        );
        cx.simulate_prompt_answer("No");
        assert!(declined.await.expect("declined checkout").is_none());
        assert!(!root.join("first.txt").exists());

        let (superseded, first) = panel.update_in(cx, begin);
        cx.condition(&panel, |_, _| prompt_context.has_pending_prompt())
            .await;
        let (replacement, duplicate) = panel.update_in(cx, begin);
        assert!(!superseded.is_current());
        assert!(replacement.is_current());
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.file_opening.pending.len(), 1)
        });
        cx.simulate_prompt_answer("Yes");
        assert!(first.await.expect("shared checkout").is_some());
        assert!(duplicate.await.expect("replacement checkout").is_some());
        assert!(!cx.has_pending_prompt());
        for path in ["first.txt", "second.txt"] {
            assert_eq!(
                std::fs::read_to_string(root.join(path)).expect("checked out file"),
                format!("{path}\n")
            );
        }

        assert!(!root.join("closed.txt").exists());

        let (closed, checkout) = panel.update_in(cx, |panel, window, cx| {
            let opening = panel.file_opening.begin(&snapshot);
            let task = panel.prepare_open(
                snapshot.clone(),
                vec!["closed.txt".into()],
                &opening,
                window,
                cx,
            );
            (opening, task)
        });
        cx.condition(&panel, |_, _| prompt_context.has_pending_prompt())
            .await;
        snapshot
            .repository()
            .lifetime()
            .expect("repository lifetime")
            .invalidate();
        panel.read_with(cx, |panel, _| panel.file_opening.cancel_closed());
        cx.simulate_prompt_answer("Yes");
        assert!(checkout.await.expect("closed checkout").is_none());
        assert!(!closed.is_current());
        assert!(!root.join("closed.txt").exists());

        let snapshot = snapshot.with_lifetime(Some(RepositoryLifetime::default()));
        let mut state = FileOpeningState::default();
        let opening = state.begin(&snapshot);
        let (release, released) = futures::channel::oneshot::channel::<()>();
        let (started, started_receiver) = futures::channel::oneshot::channel();
        let task = cx.spawn(async move |_| {
            started.send(()).expect("opening started");
            released.await?;
            anyhow::Ok(())
        });
        started_receiver.await.expect("start pending open");
        let _replacement = state.begin(&snapshot);
        assert!(
            opening
                .wait(task)
                .await
                .expect("cancel pending open")
                .is_none()
        );
        cx.run_until_parked();
        assert!(
            release.send(()).is_err(),
            "superseded task must be dropped before it can activate an editor"
        );
        drop(directory);
    }
}
