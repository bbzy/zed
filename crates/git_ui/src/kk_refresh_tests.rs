use super::*;
use futures::{FutureExt as _, future::BoxFuture};
use gpui::{BackgroundExecutor, TestAppContext};
use kk_workbench::remote::{self, Read, RepositoryInfo, Request, Response, SnapshotInfo};
use std::{
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

struct TestTransport {
    executor: BackgroundExecutor,
    roots: Vec<PathBuf>,
    barriers: HashMap<remote::ResourceId, async_channel::Receiver<()>>,
    failing: Mutex<HashSet<remote::ResourceId>>,
    discovery_barrier: Mutex<Option<async_channel::Receiver<()>>>,
    started: async_channel::Sender<remote::ResourceId>,
    active: Arc<AtomicUsize>,
    maximum: Arc<AtomicUsize>,
}

struct ActiveRequest(Arc<AtomicUsize>);

impl Drop for ActiveRequest {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl remote::Transport for TestTransport {
    fn request(&self, request: Request) -> BoxFuture<'static, anyhow::Result<Response>> {
        let roots = self.roots.clone();
        let barrier = match &request {
            Request::Snapshot { repository, .. } => self.barriers.get(repository).cloned(),
            _ => None,
        };
        let discovery_barrier = if matches!(&request, Request::Discover { .. }) {
            self.discovery_barrier
                .lock()
                .expect("discovery barrier")
                .clone()
        } else {
            None
        };
        let failing = self.failing.lock().expect("failure state").clone();
        let started = self.started.clone();
        let active = self.active.clone();
        let maximum = self.maximum.clone();
        async move {
            Ok(match request {
                Request::Discover { directory, .. } => {
                    if let Some(barrier) = discovery_barrier {
                        barrier.recv().await?;
                    }
                    Response::Discovery {
                        repositories: roots
                            .iter()
                            .enumerate()
                            .filter(|(_, root)| root.starts_with(&directory))
                            .map(|(index, root)| RepositoryInfo {
                                id: index as u128,
                                root: root.clone(),
                            })
                            .collect(),
                        errors: Vec::new(),
                    }
                }
                Request::Open { directory, .. } => Response::Repository(RepositoryInfo {
                    id: roots
                        .iter()
                        .position(|root| *root == directory)
                        .expect("known root") as u128,
                    root: directory,
                }),
                Request::Snapshot { repository, .. } => {
                    let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                    let _active = ActiveRequest(active);
                    maximum.fetch_max(current, Ordering::SeqCst);
                    started.send(repository).await?;
                    if let Some(barrier) = barrier {
                        barrier.recv().await?;
                    }
                    anyhow::ensure!(
                        !failing.contains(&repository),
                        "injected failure for repository {repository}"
                    );
                    Response::Snapshot(SnapshotInfo {
                        repository,
                        operation: format!("{repository:064x}"),
                        performance_context: None,
                    })
                }
                Request::Read {
                    operation: Read::HistoryContaining { .. },
                    ..
                } => Response::Data(remote::encode(&(
                    HistoryPage {
                        revisions: Vec::new(),
                        has_more: false,
                    },
                    Vec::<Revision>::new(),
                ))?),
                Request::ReleaseRepository { .. } | Request::DetachRepository { .. } => {
                    Response::Done
                }
                other => anyhow::bail!("unexpected request: {other:?}"),
            })
        }
        .boxed()
    }

    fn spawn(&self, future: BoxFuture<'static, ()>) {
        self.executor.spawn(future).detach();
    }

    fn timer(&self, duration: Duration) -> BoxFuture<'static, ()> {
        self.executor.timer(duration).boxed()
    }
}

async fn request(
    cx: &mut TestAppContext,
    count: usize,
    blocked: &[u128],
    failing: &[u128],
) -> (
    RefreshRequest,
    Arc<TestTransport>,
    async_channel::Receiver<u128>,
    HashMap<u128, async_channel::Sender<()>>,
) {
    let (started, starts) = async_channel::unbounded();
    let mut barriers = HashMap::new();
    let mut releases = HashMap::new();
    for id in blocked {
        let (sender, receiver) = async_channel::bounded(1);
        barriers.insert(*id, receiver);
        releases.insert(*id, sender);
    }
    let transport = Arc::new(TestTransport {
        executor: cx.background_executor.clone(),
        roots: (0..count)
            .map(|index| PathBuf::from(format!("/repos/{index}")))
            .collect(),
        barriers,
        failing: Mutex::new(failing.iter().copied().collect()),
        discovery_barrier: Mutex::new(None),
        started,
        active: Arc::new(AtomicUsize::new(0)),
        maximum: Arc::new(AtomicUsize::new(0)),
    });
    let store = KeyValueStore::open_test_db("kk_refresh").await;
    let worktree = cx.new(|_| ()).entity_id();
    (
        RefreshRequest {
            paths: vec![
                (worktree, PathBuf::from("/repos")),
                (worktree, PathBuf::from("/repos")),
            ],
            remote: Some(remote::Client::new(transport.clone())),
            executable: PathBuf::from("kk"),
            previous: Vec::new(),
            mode: HistoryMode::Lineage,
            history_limits: HashMap::new(),
            uncertain: HashMap::new(),
            comparison_store: store.into(),
            comparison_host: None,
            diagnostics: Diagnostics::default(),
            performance_context: None,
        },
        transport,
        starts,
        releases,
    )
}

#[gpui::test(iterations = 20)]
async fn kk_refresh_slow_and_failed_repositories_do_not_delay_other_results(
    cx: &mut TestAppContext,
) {
    let (request, transport, _starts, releases) = request(cx, 3, &[0], &[1]).await;
    let (output, mut events) = mpsc::unbounded();
    let worker = cx.background_spawn(request.run(output));
    let mut completed = HashMap::new();
    while completed.len() < 2 {
        if let RefreshEvent::Loaded(completion) = events.next().await.expect("event") {
            let RepositoryRefresh { root, view, .. } = *completion;
            completed.insert(root, view.is_ok());
        }
    }
    assert_eq!(completed.get(Path::new("/repos/1")), Some(&false));
    assert_eq!(completed.get(Path::new("/repos/2")), Some(&true));
    assert!(!completed.contains_key(Path::new("/repos/0")));
    releases[&0]
        .send(())
        .await
        .expect("release first repository");
    while let Some(event) = events.next().await {
        if let RefreshEvent::Loaded(completion) = event {
            let RepositoryRefresh { root, view, .. } = *completion;
            assert_eq!(root, Path::new("/repos/0"));
            view.expect("loaded repository");
            assert!(
                completed.insert(root, true).is_none(),
                "duplicate repository load"
            );
        }
    }
    worker.await;
    assert_eq!(completed.len(), 3);
    assert!(transport.maximum.load(Ordering::SeqCst) <= CONCURRENCY);
}

#[gpui::test(iterations = 20)]
async fn kk_refresh_limits_concurrency_and_runs_queued_repositories(cx: &mut TestAppContext) {
    let (request, transport, starts, releases) = request(cx, 7, &[0, 1, 2, 3], &[]).await;
    let (output, mut events) = mpsc::unbounded();
    let worker = cx.background_spawn(request.run(output));
    for _ in 0..CONCURRENCY {
        starts.recv().await.expect("started repository");
    }
    cx.run_until_parked();
    assert!(
        starts.try_recv().is_err(),
        "fifth repository started before a slot was free"
    );
    assert_eq!(transport.maximum.load(Ordering::SeqCst), CONCURRENCY);
    releases[&0].send(()).await.expect("release slot");
    assert!(starts.recv().await.expect("queued repository") >= 4);
    for id in 1..4 {
        releases[&id]
            .send(())
            .await
            .expect("release remaining repository");
    }
    let mut completed = HashSet::new();
    while let Some(event) = events.next().await {
        if let RefreshEvent::Loaded(completion) = event {
            let RepositoryRefresh { root, view, .. } = *completion;
            view.expect("loaded repository");
            assert!(completed.insert(root), "duplicate load");
        }
    }
    worker.await;
    assert_eq!(completed.len(), 7);
    assert_eq!(transport.maximum.load(Ordering::SeqCst), CONCURRENCY);
}

#[gpui::test(iterations = 20)]
async fn kk_refresh_cached_repositories_load_before_discovery_finishes(cx: &mut TestAppContext) {
    let (mut request, transport, _starts, _releases) = request(cx, 1, &[], &[]).await;
    let client = request.remote.as_ref().expect("remote client");
    let repository = client
        .open(
            &transport.roots[0],
            &request.executable,
            Diagnostics::default(),
        )
        .await
        .expect("repository");
    let snapshot = repository.snapshot(true).await.expect("snapshot");
    request.previous.push(RepositoryView {
        history_snapshot: None,
        mode: HistoryMode::Lineage,
        stale: false,
        snapshot,
        history: HistoryPage {
            revisions: Vec::new(),
            has_more: false,
        },
        expanded: HashMap::new(),
        comparison: None,
        history_targets: Vec::new(),
    });
    let (release, blocked) = async_channel::unbounded();
    *transport
        .discovery_barrier
        .lock()
        .expect("discovery barrier") = Some(blocked);
    let (output, mut events) = mpsc::unbounded();
    let worker = cx.background_spawn(request.run(output));
    let Some(RefreshEvent::Loaded(completion)) = events.next().await else {
        panic!("cached repository did not finish first")
    };
    completion.view.expect("loaded cached repository");
    // Two overlapping discovery paths are deliberately blocked while the cached repo completes.
    release.send(()).await.expect("release discovery");
    release
        .send(())
        .await
        .expect("release overlapping discovery");
    while let Some(event) = events.next().await {
        assert!(matches!(event, RefreshEvent::Discovered { .. }));
    }
    worker.await;
}
