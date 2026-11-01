use super::*;
use crate::remote::{self, Failure, Mutation, Read, Request, Response, Service, SnapshotInfo};
use futures::FutureExt as _;
use serde::de::DeserializeOwned;
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

struct ServiceTransport {
    service: Service,
    server_root: PathBuf,
    remote_root: PathBuf,
    capture_response: Mutex<Option<smol::channel::Receiver<()>>>,
    discovery_response: Mutex<Option<smol::channel::Receiver<()>>>,
    released_repositories: (
        smol::channel::Sender<remote::ResourceId>,
        smol::channel::Receiver<remote::ResourceId>,
    ),
    timer_ticks: (smol::channel::Sender<()>, smol::channel::Receiver<()>),
    lose_publication: AtomicBool,
}

#[test]
fn kk_merge_diff_base_uses_both_parents_locally_and_remotely() -> Result<()> {
    smol::block_on(async {
        let fixture = Fixture::create()?;
        let original = "first\none\ntwo\nthree\nfour\nfive\nlast\n";
        let unusual_path = if cfg!(windows) {
            "special.txt"
        } else {
            "quote\"and\\slash.txt"
        };
        let original_pointer = LfsPointer {
            hash: "a".repeat(64),
            size: 10,
        }
        .text();
        for (path, bytes) in [
            ("shared.txt", original.as_bytes()),
            ("deleted.txt", b"deleted\n"),
            ("old [name].txt", b"rename me\n"),
            ("binary.bin", b"old\0binary"),
            ("crlf.txt", b"old\r\nlast"),
            ("empty.txt", b""),
            (unusual_path, b"before\n"),
            ("pointer.txt", original_pointer.as_bytes()),
        ] {
            fs::write(fixture.root().join(path), bytes)?;
        }
        fixture.run(&["new", "-m", "left"])?;
        fs::write(
            fixture.root().join("shared.txt"),
            original.replace("first", "left"),
        )?;
        let left = fixture.run(&["log", "-r", "@", "--no-graph", "-T", "change_id"])?;
        fixture.run(&["new", "@-", "-m", "right"])?;
        fs::write(
            fixture.root().join("shared.txt"),
            original.replace("last", "right"),
        )?;
        fixture.run(&["new", "@", &left, "-m", "merge"])?;
        let merged = original.replace("first", "left").replace("last", "right");
        assert_eq!(
            fs::read_to_string(fixture.root().join("shared.txt"))?,
            merged
        );
        fs::write(
            fixture.root().join("shared.txt"),
            merged.replace("three", "merged"),
        )?;
        fs::remove_file(fixture.root().join("deleted.txt"))?;
        fs::rename(
            fixture.root().join("old [name].txt"),
            fixture.root().join("new [name].txt"),
        )?;
        fs::write(fixture.root().join("added.txt"), "added\n")?;
        fs::write(fixture.root().join("binary.bin"), b"new\0binary")?;
        fs::write(fixture.root().join("crlf.txt"), b"new\r\nlast")?;
        fs::write(fixture.root().join("empty.txt"), b"not empty")?;
        fs::write(fixture.root().join(unusual_path), b"after\n")?;
        fs::write(
            fixture.root().join("pointer.txt"),
            LfsPointer {
                hash: "b".repeat(64),
                size: 20,
            }
            .text(),
        )?;
        let local = fixture.repository().await?;
        local.snapshot(true).await?;
        let transport = ServiceTransport::new(&fixture)?;
        let remote = transport.open().await?;
        for repository in [&local, &remote] {
            let snapshot = repository.snapshot(false).await?;
            let revision = snapshot.exact_revision("@").await?;
            assert_eq!(revision.parent_commit_ids.len(), 2);
            let files = snapshot
                .changed_files(&DiffScope::Change(revision.full_commit_id.clone()))
                .await?;
            for (path, expected) in [
                ("shared.txt", Some(merged.as_bytes())),
                ("deleted.txt", Some(b"deleted\n".as_slice())),
                ("new [name].txt", Some(b"rename me\n".as_slice())),
                ("crlf.txt", Some(b"old\r\nlast".as_slice())),
                ("empty.txt", Some(b"".as_slice())),
                ("added.txt", None),
                (unusual_path, Some(b"before\n".as_slice())),
                ("pointer.txt", Some(original_pointer.as_bytes())),
            ] {
                let file = files
                    .iter()
                    .find(|file| file.path == path)
                    .context("missing changed file")?;
                let (information, contents) = snapshot
                    .merged_parent_file(&revision.full_commit_id, file)
                    .await?;
                assert_eq!(contents.as_deref(), expected, "{path}");
                assert_eq!(information.exists, Some(expected.is_some()), "{path}");
                assert!(
                    information.unavailable.is_none(),
                    "{path}: {:?}",
                    information.unavailable
                );
                if path == "pointer.txt" {
                    assert_eq!(
                        information.pointer.map(|pointer| pointer.hash),
                        Some("a".repeat(64))
                    );
                }
            }
            let binary = files
                .iter()
                .find(|file| file.path == "binary.bin")
                .context("missing binary")?;
            let (information, contents) = snapshot
                .merged_parent_file(&revision.full_commit_id, binary)
                .await?;
            assert!(contents.is_none());
            assert_eq!(information.exists, Some(true));
            assert!(
                information
                    .unavailable
                    .as_deref()
                    .is_some_and(|error| error.contains("binary contents"))
            );
            assert_eq!(
                snapshot.operation(),
                repository.snapshot(false).await?.operation()
            );
        }

        Ok(())
    })
}

impl ServiceTransport {
    fn new(fixture: &Fixture) -> Result<Arc<Self>> {
        Ok(Arc::new(Self {
            service: Service::default(),
            server_root: fs::canonicalize(fixture.root())?,
            remote_root: PathBuf::from(format!("/remote-kk-{}", uuid::Uuid::new_v4())),
            capture_response: Mutex::new(None),
            discovery_response: Mutex::new(None),
            released_repositories: smol::channel::unbounded(),
            timer_ticks: smol::channel::bounded(1),
            lose_publication: AtomicBool::new(false),
        }))
    }

    async fn open(self: &Arc<Self>) -> Result<Repository> {
        remote::Client::new(self.clone())
            .open(
                &self.remote_root,
                Path::new("/not-installed-on-client/kk"),
                Diagnostics::default(),
            )
            .await
    }
}

impl remote::Transport for ServiceTransport {
    fn request(
        &self,
        mut request: Request,
    ) -> futures::future::BoxFuture<'static, Result<Response>> {
        let publication = matches!(request, Request::Publish { .. });
        let capture = matches!(request, Request::CaptureSaved { .. });
        let discovery = matches!(request, Request::Open { .. } | Request::Discover { .. });
        let released = match &request {
            Request::ReleaseRepository { repository } => Some(*repository),
            _ => None,
        };
        let conflict_response = matches!(
            request,
            Request::Read {
                operation: Read::FileConflict { .. } | Read::RefreshConflictSources { .. },
                ..
            }
        );
        match &mut request {
            Request::Open {
                directory,
                executable,
            }
            | Request::Discover {
                directory,
                executable,
            } => {
                assert_eq!(*directory, self.remote_root);
                *directory = self.server_root.clone();
                *executable = "kk".into();
            }
            Request::RelativeFilePath { path, .. } => {
                *path = self
                    .server_root
                    .join(path.strip_prefix(&self.remote_root).expect("remote path"));
            }
            Request::Read {
                operation: Read::RefreshConflictSources { conflict },
                ..
            }
            | Request::Prepare {
                mutation: Mutation::ResolveConflict { conflict, .. },
                ..
            } => {
                assert_eq!(conflict.repository_root, self.remote_root);
                conflict.repository_root = self.server_root.clone();
            }
            _ => {}
        }

        let request = remote::decode(&remote::encode(&request).expect("encode request"))
            .expect("decode request");
        let response = self.service.execute(request);
        let remote_root = self.remote_root.clone();
        let barrier = if capture {
            self.capture_response
                .lock()
                .expect("capture barrier")
                .take()
        } else if discovery {
            self.discovery_response
                .lock()
                .expect("discovery barrier")
                .take()
        } else {
            None
        };
        let lose = publication && self.lose_publication.swap(false, Ordering::AcqRel);
        let releases = self.released_repositories.0.clone();
        async move {
            let mut response = response.await?;
            if let Some(repository) = released {
                releases.send(repository).await?;
            }

            if let Response::Repository(info) = &mut response {
                info.root = remote_root.clone();
            }

            if let Response::Discovery { repositories, .. } = &mut response {
                for info in repositories {
                    info.root = remote_root.clone();
                }
            }

            if conflict_response {
                let Response::Data(bytes) = &mut response else {
                    anyhow::bail!("Expected conflict response");
                };
                let mut conflict: FileConflict = remote::decode(bytes)?;
                conflict.repository_root = remote_root;
                *bytes = remote::encode(&conflict)?;
            }

            if let Some(barrier) = barrier {
                barrier.recv().await?;
            }

            ensure!(!lose, "Connection lost after publication");
            remote::decode(&remote::encode(&response)?)
        }
        .boxed()
    }

    fn spawn(&self, future: futures::future::BoxFuture<'static, ()>) {
        smol::spawn(future).detach();
    }
    fn timer(&self, _duration: std::time::Duration) -> futures::future::BoxFuture<'static, ()> {
        let ticks = self.timer_ticks.1.clone();
        async move {
            ticks.recv().await.expect("advance test timer");
        }
        .boxed()
    }
}

#[track_caller]
fn same_data<T: serde::Serialize>(actual: T, expected: T) -> Result<()> {
    assert_eq!(
        serde_json::to_value(actual)?,
        serde_json::to_value(expected)?
    );
    Ok(())
}

#[test]
fn cancelled_remote_discovery_releases_handles_after_the_response_arrives() -> Result<()> {
    smol::block_on(async {
        let fixture = changed_fixture()?;
        let transport = ServiceTransport::new(&fixture)?;
        let client = remote::Client::new(transport.clone());
        for discover in [false, true] {
            let (release, response) = smol::channel::bounded(1);
            *transport
                .discovery_response
                .lock()
                .expect("discovery barrier") = Some(response);
            let executable = Path::new("/not-installed-on-client/kk");
            let mut opening = if discover {
                client
                    .discover(&transport.remote_root, executable, Diagnostics::default())
                    .map(|result| result.map(drop))
                    .boxed()
            } else {
                client
                    .open(&transport.remote_root, executable, Diagnostics::default())
                    .map(|result| result.map(drop))
                    .boxed()
            };
            assert!(futures::poll!(&mut opening).is_pending());
            drop(opening);
            release.send(()).await?;
            let released = transport.released_repositories.1.recv().await?;
            assert!(snapshot(&transport.service, released, true).await.is_err());
        }

        let repository = transport.open().await?;
        assert_eq!(
            repository
                .snapshot(true)
                .await?
                .file_content("@", "file.txt")
                .await?,
            b"changed\n"
        );
        Ok(())
    })
}

#[test]
fn stale_workspaces_remain_readable_and_recover_locally_and_remotely() -> Result<()> {
    smol::block_on(async {
        for remote in [false, true] {
            let fixture = changed_fixture()?;
            fixture.run(&[
                "config",
                "set",
                "--repo",
                "snapshot.auto-update-stale",
                "false",
            ])?;
            let transport = ServiceTransport::new(&fixture)?;
            let repository = if remote {
                transport.open().await?
            } else {
                fixture.repository().await?
            };
            let original = repository.snapshot(true).await?;
            fixture.run(&["new", "root()", "--ignore-working-copy"])?;
            let error = repository
                .snapshot(true)
                .await
                .expect_err("stale working copy");
            assert!(is_stale_workspace(&error), "{error:#}");
            assert!(repository.snapshot(false).await.is_ok());
            let discovered = discover(fixture.root(), Path::new("kk")).await;
            assert_eq!(discovered.repositories.len(), 1);
            assert!(discovered.errors.is_empty());
            let updated = repository.update_stale_workspace().await?;
            assert!(
                updated
                    .file_content_if_exists("@", "file.txt")
                    .await?
                    .is_none()
            );
            assert!(!fixture.root().join("file.txt").exists());
            assert_eq!(
                repository.snapshot(true).await?.operation(),
                updated.operation()
            );
            assert_eq!(original.file_content("@", "file.txt").await?, b"changed\n");
            let lifetime = RepositoryLifetime::default();
            let closed = updated.with_lifetime(Some(lifetime.clone()));
            lifetime.invalidate();
            assert!(closed.repository().update_stale_workspace().await.is_err());
        }

        Ok(())
    })
}

#[test]
fn remote_draft_export_preserves_exact_text_and_reports_write_errors() -> Result<()> {
    smol::block_on(async {
        let fixture = changed_fixture()?;
        let transport = ServiceTransport::new(&fixture)?;
        let client = remote::Client::new(transport.clone());
        let path = fixture.root().join("resolution.resolved");
        for text in ["\u{feff}first\r\nsecond\nlast\r", ""] {
            client.export_draft(path.clone(), text.into()).await?;
            assert_eq!(fs::read(&path)?, text.as_bytes());
        }

        assert!(
            client
                .export_draft(fixture.root().join("missing/parent/file"), "text".into())
                .await
                .is_err()
        );
        assert!(
            client
                .export_draft(PathBuf::from("relative.resolved"), "text".into())
                .await
                .is_err()
        );
        transport.service.close()?;
        assert!(
            client
                .export_draft(path.clone(), "closed".into())
                .await
                .is_err()
        );
        assert!(fs::read(path)?.is_empty());
        Ok(())
    })
}

#[test]
fn remote_diagnostics_wait_for_output_and_reset_after_reconnect() -> Result<()> {
    smol::block_on(async {
        let fixture = changed_fixture()?;
        let transport = ServiceTransport::new(&fixture)?;
        let client = remote::Client::new(transport.clone());
        client.configure_diagnostics(vec!["perf".into()]).await?;
        let repository = transport.open().await?;
        let initial = client.diagnostics(None, false).await?;
        assert!(initial.text.contains("[COMMAND]"));
        assert!(
            client
                .diagnostics(Some(initial.cursor), false)
                .await?
                .text
                .is_empty()
        );
        let mut waiting = Box::pin(client.diagnostics(Some(initial.cursor), true));
        assert!(futures::poll!(&mut waiting).is_pending());
        repository.snapshot(true).await?;
        assert!(!waiting.await?.text.is_empty());
        let last = client.diagnostics(None, false).await?;
        assert!(last.text.contains("process.kk"));
        let mut closing = Box::pin(client.diagnostics(Some(last.cursor), true));
        assert!(futures::poll!(&mut closing).is_pending());
        transport.service.close()?;
        assert!(closing.await.is_err());

        let reconnected = Service::default();
        open(&reconnected, &fixture).await?;
        let Response::Data(bytes) = request(
            &reconnected,
            Request::Diagnostics {
                cursor: Some(last.cursor),
                wait: true,
            },
        )
        .await?
        else {
            anyhow::bail!("Expected new diagnostic stream");
        };
        let output: remote::DiagnosticOutput = remote::decode(&bytes)?;
        assert!(output.text.contains("[COMMAND]"));
        assert_ne!(output.cursor, last.cursor);
        reconnected.close()?;
        Ok(())
    })
}

#[test]
fn remote_repository_handles_read_and_publish_without_client_files_or_executable() -> Result<()> {
    smol::block_on(async {
        let fixture = changed_fixture()?;
        let transport = ServiceTransport::new(&fixture)?;
        let repository = transport.open().await?;
        assert!(repository.is_remote());
        assert!(!repository.root().exists());
        let snapshot = repository.snapshot(true).await?;
        let local = fixture.repository().await?.snapshot(false).await?;
        let working = local.exact_revision("@").await?;
        let scope = DiffScope::Change("@".into());
        same_data(
            snapshot.revisions(None, 20).await?,
            local.revisions(None, 20).await?,
        )?;
        same_data(
            snapshot.history(HistoryMode::Lineage, 50).await?,
            local.history(HistoryMode::Lineage, 50).await?,
        )?;
        same_data(
            snapshot
                .history_containing(HistoryMode::Lineage, 50, &working)
                .await?,
            local
                .history_containing(HistoryMode::Lineage, 50, &working)
                .await?,
        )?;
        same_data(snapshot.exact_revision("@").await?, working.clone())?;
        same_data(snapshot.resolve_revision(&working).await?, working.clone())?;
        same_data(
            snapshot.comparison_candidates().await?,
            local.comparison_candidates().await?,
        )?;
        same_data(
            snapshot.comparison_boundary("@-", None).await?,
            local.comparison_boundary("@-", None).await?,
        )?;
        same_data(
            snapshot.category_commits(&working).await?,
            local.category_commits(&working).await?,
        )?;
        same_data(
            snapshot
                .commit_emptiness(std::slice::from_ref(&working.full_commit_id))
                .await?,
            local
                .commit_emptiness(std::slice::from_ref(&working.full_commit_id))
                .await?,
        )?;
        same_data(
            snapshot.search_revisions("source", None, true, 50).await?,
            local.search_revisions("source", None, true, 50).await?,
        )?;
        same_data(
            snapshot.changed_files(&scope).await?,
            local.changed_files(&scope).await?,
        )?;
        same_data(
            snapshot.diff(&scope, Some("file.txt")).await?,
            local.diff(&scope, Some("file.txt")).await?,
        )?;
        same_data(
            snapshot.search_data(&scope).await?,
            local.search_data(&scope).await?,
        )?;
        same_data(
            snapshot.annotate("@", "file.txt").await?,
            local.annotate("@", "file.txt").await?,
        )?;
        same_data(
            snapshot.file_information("@", "file.txt").await?,
            local.file_information("@", "file.txt").await?,
        )?;
        assert_eq!(snapshot.file_content("@", "file.txt").await?, b"changed\n");
        assert_eq!(
            snapshot.file_content_if_exists("@", "missing.txt").await?,
            None
        );
        assert_eq!(
            snapshot.current_file_path("@", "file.txt").await?,
            Some("file.txt".into())
        );
        assert_eq!(
            repository.working_file_content("file.txt").await?,
            b"changed\n"
        );
        same_data(
            repository
                .working_file_information("file.txt", None)
                .await?,
            local
                .repository()
                .working_file_information("file.txt", None)
                .await?,
        )?;
        same_data(
            repository
                .working_file_information("file.txt", Some(b"draft"))
                .await?,
            local
                .repository()
                .working_file_information("file.txt", Some(b"draft"))
                .await?,
        )?;
        assert_eq!(
            repository
                .relative_file_path(&repository.root().join("file.txt"))
                .await?,
            "file.txt"
        );
        assert!(
            repository
                .excluded_sparse_files(&["file.txt".into()])
                .await?
                .is_empty()
        );
        assert!(matches!(
            repository
                .expand_sparse_checkout(&["file.txt".into()], || true)
                .await?,
            SparseCheckout::Ready(_)
        ));
        let file = snapshot.changed_files(&scope).await?.remove(0);
        snapshot
            .verify_file_version(&working.full_commit_id, &working.full_commit_id, &file)
            .await?;
        let prepared = snapshot.prepare_file_revert(&scope, &file).await?;
        assert_eq!(
            prepared.snapshot().file_content("@", "file.txt").await?,
            b"base\n"
        );
        drop(prepared);
        let current = repository.snapshot_before_write().await?;
        assert_eq!(current.operation(), snapshot.operation());
        let publication = current
            .prepare_file_revert(&scope, &file)
            .await?
            .publish()
            .await?;
        assert_eq!(
            publication.receipt.status().await?,
            PublicationStatus::Applied
        );
        publication.receipt.synchronize().await?;
        assert_eq!(fs::read(fixture.root().join("file.txt"))?, b"base\n");
        assert_eq!(snapshot.file_content("@", "file.txt").await?, b"changed\n");
        fixture.run(&["undo"])?;
        assert_eq!(fs::read(fixture.root().join("file.txt"))?, b"changed\n");
        Ok(())
    })
}

#[test]
fn remote_saved_handles_capture_eagerly_and_reconcile_a_lost_publication_response() -> Result<()> {
    smol::block_on(async {
        let fixture = changed_fixture()?;
        let transport = ServiceTransport::new(&fixture)?;
        let repository = transport.open().await?;
        let original = repository.snapshot(true).await?;
        let classification = ClassificationRequest {
            comparison: original.exact_revision("@-").await?.revset(),
            range_start: None,
            source: None,
            target: ClassificationTarget::Category(Category::Reviewed),
            files: original
                .changed_files(&DiffScope::Change("@".into()))
                .await?
                .into_iter()
                .map(|file| FileSelection { file, ranges: None })
                .collect(),
        };
        let (release, response) = smol::channel::bounded(1);
        *transport.capture_response.lock().expect("capture barrier") = Some(response);
        fs::write(fixture.root().join("file.txt"), b"captured\r\n")?;
        let captured = SavedSnapshot::capture(&repository, ["file.txt".into()])?;
        assert_eq!(captured.version(), None);
        fs::write(fixture.root().join("file.txt"), b"later\r\n")?;
        transport
            .service
            .record_saved_file(&fixture.root().join("file.txt"))?;
        let error = repository
            .snapshot_saved_working_copy()
            .await
            .expect_err("remote review rejects deferred saved contents");
        assert!(
            error.to_string().contains("pending classification"),
            "{error}"
        );
        assert_eq!(
            repository.snapshot(false).await?.operation(),
            original.operation()
        );
        release.send(()).await?;
        let refreshed_repository = transport.open().await?;
        let refreshed = refreshed_repository.snapshot_before_write().await?;
        let prepared = refreshed
            .prepare_classification_with_saved(&original, &classification, vec![captured.clone()])
            .await?
            .context("prepared classification")?;
        assert!(captured.version().is_some());
        assert_eq!(
            captured.files()?["file.txt"].bytes.as_deref(),
            Some(b"captured\r\n".as_slice())
        );
        let destination = prepared.destination().clone();
        let prepared_receipt = prepared.receipt().await?;
        assert_eq!(
            prepared_receipt.status().await?,
            PublicationStatus::Rejected
        );
        transport.lose_publication.store(true, Ordering::Release);
        let error = prepared
            .publish()
            .await
            .expect_err("lost response must not be success");
        let receipt = error
            .downcast_ref::<UncertainPublication>()
            .context("uncertain publication retains recovery receipt")?
            .receipt
            .clone();
        let reopened = transport.open().await?;
        let receipt = receipt.with_repository(&reopened)?;
        assert_eq!(receipt.status().await?, PublicationStatus::Applied);
        assert_eq!(prepared_receipt.status().await?, PublicationStatus::Applied);
        let published = receipt.synchronize().await?;
        assert_eq!(
            published
                .file_content(&destination.revset(), "file.txt")
                .await?,
            b"captured\r\n"
        );
        assert_eq!(fs::read(fixture.root().join("file.txt"))?, b"later\r\n");
        let finished = captured.finish().await?.context("later saved snapshot")?;
        assert_eq!(finished.file_content("@", "file.txt").await?, b"later\r\n");
        assert_eq!(
            repository
                .snapshot_saved_working_copy()
                .await?
                .file_content("@", "file.txt")
                .await?,
            b"later\r\n"
        );
        assert_eq!(original.file_content("@", "file.txt").await?, b"changed\n");
        Ok(())
    })
}

#[test]
fn remote_lifetimes_close_only_their_handles_and_cancel_pending_sparse_checkouts() -> Result<()> {
    smol::block_on(async {
        let fixture = changed_fixture()?;
        let transport = ServiceTransport::new(&fixture)?;
        let first = transport.open().await?;
        let second = transport.open().await?;
        let lifetime = RepositoryLifetime::default();
        let original = first
            .snapshot(true)
            .await?
            .with_lifetime(Some(lifetime.clone()));
        let scope = DiffScope::Change("@".into());
        let file = original.changed_files(&scope).await?.remove(0);
        let prepared = original.prepare_file_revert(&scope, &file).await?;
        lifetime.invalidate();
        assert!(prepared.publish().await.is_err());
        assert!(original.repository().snapshot(true).await.is_err());
        assert_eq!(original.file_content("@", "file.txt").await?, b"changed\n");
        let current = second.snapshot_before_write().await?;
        current
            .prepare_file_revert(&scope, &file)
            .await?
            .publish()
            .await?;
        assert_eq!(fs::read(fixture.root().join("file.txt"))?, b"base\n");

        fixture.run(&["new"])?;
        fixture.run(&["sparse", "set", "--clear", "--add", "visible.txt"])?;
        let guard = crate::transaction::write_lock(&transport.server_root).await?;
        let active = AtomicBool::new(true);
        let paths = ["file.txt".into()];
        let mut checkout =
            Box::pin(second.expand_sparse_checkout(&paths, || active.load(Ordering::Acquire)));
        assert!(futures::poll!(&mut checkout).is_pending());
        active.store(false, Ordering::Release);
        transport.timer_ticks.0.send(()).await?;
        assert!(matches!(checkout.await?, SparseCheckout::Cancelled));
        drop(guard);
        assert!(!fixture.root().join("file.txt").exists());
        assert!(matches!(
            second
                .expand_sparse_checkout(&["file.txt".into()], || true)
                .await?,
            SparseCheckout::Ready(_)
        ));
        assert_eq!(fs::read(fixture.root().join("file.txt"))?, b"base\n");
        Ok(())
    })
}

#[test]
fn remote_file_moves_preserve_other_files_and_undo_as_one_operation() -> Result<()> {
    smol::block_on(async {
        for direction in [FileMoveDirection::Parent, FileMoveDirection::Child] {
            let fixture = changed_fixture()?;
            fs::write(fixture.root().join("stay.txt"), b"stay\n")?;
            fixture.run(&["new", "-m", "head"])?;
            let transport = ServiceTransport::new(&fixture)?;
            let repository = transport.open().await?;
            let before = repository.snapshot(true).await?;
            let source = before.exact_revision("@-").await?;
            let destination = before
                .exact_revision(match direction {
                    FileMoveDirection::Parent => "@--",
                    FileMoveDirection::Child => "@",
                })
                .await?;
            let file = before
                .changed_files(&DiffScope::Change(source.revset()))
                .await?
                .into_iter()
                .find(|file| file.path == "file.txt")
                .context("selected file")?;
            let prepared = before.prepare_file_move(&source, direction, &file).await?;
            assert_eq!(
                repository.snapshot(false).await?.operation(),
                before.operation()
            );
            let publication = prepared.publish().await?;
            assert_eq!(
                publication.receipt.status().await?,
                PublicationStatus::Applied
            );
            assert_eq!(
                publication
                    .snapshot
                    .file_content(&destination.revset(), "file.txt")
                    .await?,
                b"changed\n"
            );
            assert_eq!(
                publication
                    .snapshot
                    .file_content(&source.revset(), "stay.txt")
                    .await?,
                b"stay\n"
            );
            assert!(
                publication
                    .snapshot
                    .changed_files(&DiffScope::Change(source.revset()))
                    .await?
                    .iter()
                    .all(|file| file.path != "file.txt")
            );
            fixture.run(&["undo"])?;
            assert_eq!(
                repository
                    .snapshot(false)
                    .await?
                    .exact_revision(&source.revset())
                    .await?
                    .full_commit_id,
                source.full_commit_id
            );
            assert_eq!(
                before.file_content(&source.revset(), "file.txt").await?,
                b"changed\n"
            );
        }

        Ok(())
    })
}

#[test]
fn remote_conflict_resolution_preserves_exact_inputs_and_opaque_results() -> Result<()> {
    smol::block_on(async {
        let path = "quoted\" file.txt";
        for binary in [false, true] {
            let prefix = if binary {
                b"\0".as_slice()
            } else {
                b"header\r\n".as_slice()
            };
            let fixture = super::conflict_tests::conflict_fixture(
                &[prefix, b"base\r\n"].concat(),
                &[prefix, b"left\r\n"].concat(),
                &[prefix, b"right\r\n"].concat(),
                path,
            )?;
            let transport = ServiceTransport::new(&fixture)?;
            let repository = transport.open().await?;
            let before = repository.snapshot(true).await?;
            let mut conflict = before.file_conflict("@", path).await?;
            assert_eq!(conflict.binary, binary);
            assert_eq!(conflict.repository_root, repository.root());
            assert_eq!(before.conflicted_paths("@").await?, [path]);
            let inputs = conflict.inputs.clone();
            before.refresh_conflict_sources(&mut conflict).await?;
            assert_eq!(inputs, conflict.inputs);
            let result = [prefix, b"resolved\r\n"].concat();
            let prepared = before
                .prepare_conflict_resolution(
                    &conflict,
                    &conflict.commit_id,
                    Some(&result),
                    Some(true),
                )
                .await?;
            assert_eq!(prepared.snapshot().file_content("@", path).await?, result);
            assert_eq!(
                repository.snapshot(false).await?.operation(),
                before.operation()
            );
            let publication = prepared.publish().await?;
            assert_eq!(
                publication.receipt.status().await?,
                PublicationStatus::Applied
            );
            assert!(publication.snapshot.conflicted_paths("@").await?.is_empty());
            assert_eq!(fs::read(fixture.root().join(path))?, result);
            assert_eq!(
                fs::read(fixture.root().join("unselected.txt"))?,
                b"unchanged\n"
            );
            fixture.run(&["undo"])?;
            assert_eq!(
                repository
                    .snapshot(false)
                    .await?
                    .file_conflict("@", path)
                    .await?
                    .inputs,
                inputs
            );
            assert_eq!(
                before.file_conflict("@", path).await?.materialized,
                conflict.materialized
            );
        }

        Ok(())
    })
}

async fn request(service: &Service, request: Request) -> Result<Response> {
    let request = remote::decode(&remote::encode(&request)?)?;
    let response = service.execute(request).await;
    let response: std::result::Result<Response, Failure> =
        remote::decode(&remote::encode(&response)?)?;
    Ok(response?)
}

async fn open(service: &Service, fixture: &Fixture) -> Result<remote::ResourceId> {
    let Response::Repository(repository) = request(
        service,
        Request::Open {
            directory: fixture.root().to_owned(),
            executable: "kk".into(),
        },
    )
    .await?
    else {
        anyhow::bail!("Expected repository");
    };
    assert_eq!(repository.root, fs::canonicalize(fixture.root())?);
    Ok(repository.id)
}

async fn snapshot(
    service: &Service,
    repository: remote::ResourceId,
    capture: bool,
) -> Result<SnapshotInfo> {
    let Response::Snapshot(snapshot) = request(
        service,
        Request::Snapshot {
            repository,
            capture,
        },
    )
    .await?
    else {
        anyhow::bail!("Expected snapshot");
    };
    Ok(snapshot)
}

async fn read<T: DeserializeOwned>(
    service: &Service,
    snapshot: &SnapshotInfo,
    operation: Read,
) -> Result<T> {
    let Response::Data(data) = request(
        service,
        Request::Read {
            snapshot: snapshot.clone(),
            operation,
        },
    )
    .await?
    else {
        anyhow::bail!("Expected data");
    };
    remote::decode(&data)
}

async fn publication_status(
    service: &Service,
    receipt: &remote::Receipt,
) -> Result<PublicationStatus> {
    let Response::Data(status) = request(
        service,
        Request::PublicationStatus {
            receipt: receipt.clone(),
        },
    )
    .await?
    else {
        anyhow::bail!("Expected publication status");
    };
    remote::decode(&status)
}

fn changed_fixture() -> Result<Fixture> {
    let fixture = Fixture::create()?;
    fs::write(fixture.root().join("file.txt"), "base\n")?;
    fixture.run(&["new", "-m", "source"])?;
    fs::write(fixture.root().join("file.txt"), "changed\n")?;
    Ok(fixture)
}

async fn revert(
    service: &Service,
    snapshot: &SnapshotInfo,
    id: remote::ResourceId,
) -> Result<Request> {
    let scope = DiffScope::Change("@".into());
    let files: Vec<ChangedFile> = read(
        service,
        snapshot,
        Read::ChangedFiles {
            scope: scope.clone(),
        },
    )
    .await?;
    let file = files
        .into_iter()
        .find(|file| file.path == "file.txt")
        .context("Changed file")?;
    Ok(Request::Prepare {
        id,
        snapshot: snapshot.clone(),
        mutation: Mutation::Revert { scope, file },
    })
}

#[test]
fn remote_diff_opening_release_before_begin_and_root_close_unblock_split() -> Result<()> {
    smol::block_on(async {
        let fixture = changed_fixture()?;
        let service = Service::default();
        let repository = open(&service, &fixture).await?;
        let mut original = snapshot(&service, repository, true).await?;
        request(
            &service,
            Request::ConfigureDiagnostics {
                categories: vec!["perf".into()],
            },
        )
        .await?;
        let client_diagnostics = Diagnostics::default();
        client_diagnostics.configure(&["perf".into()]);
        let client_span = client_diagnostics.start("client.diff", None, serde_json::Value::Null);
        original.performance_context = client_span.context().cloned();
        request(&service, Request::EndDiffOpening { id: 200 }).await?;
        request(
            &service,
            Request::BeginDiffOpening {
                repository,
                id: 200,
            },
        )
        .await?;
        let files: Vec<ChangedFile> = read(
            &service,
            &original,
            Read::ChangedFiles {
                scope: DiffScope::Change("@".into()),
            },
        )
        .await?;
        let prepared = Request::Prepare {
            id: 201,
            snapshot: original.clone(),
            mutation: Mutation::Classify {
                previous: original.clone(),
                saved: Vec::new(),
                request: ClassificationRequest {
                    comparison: "@-".into(),
                    range_start: None,
                    source: None,
                    target: ClassificationTarget::Category(Category::Reviewed),
                    files: files
                        .into_iter()
                        .map(|file| FileSelection { file, ranges: None })
                        .collect(),
                },
            },
        };
        let response = request(&service, prepared).await?;
        assert!(matches!(response, Response::Prepared { .. }));
        request(&service, Request::ReleasePrepared { prepared: 201 }).await?;
        request(
            &service,
            Request::BeginDiffOpening {
                repository,
                id: 202,
            },
        )
        .await?;
        let contents: Vec<u8> = read(
            &service,
            &original,
            Read::FileContent {
                revision: "@".into(),
                path: "file.txt".into(),
            },
        )
        .await?;
        assert_eq!(contents, b"changed\n");
        let Response::Data(log) = request(
            &service,
            Request::Diagnostics {
                cursor: None,
                wait: false,
            },
        )
        .await?
        else {
            anyhow::bail!("Expected diagnostics");
        };
        let log: remote::DiagnosticOutput = remote::decode(&log)?;
        assert!(log.text.contains("upstreamContext") && log.text.contains("remote.bindSnapshot"));
        assert!(!log.text.contains("changed\\n"));
        client_span.finish("completed", serde_json::Value::Null);
        request(&service, Request::DetachRepository { repository }).await?;
        request(&service, Request::EndDiffOpening { id: 202 }).await?;
        assert_eq!(
            fixture
                .repository()
                .await?
                .snapshot(false)
                .await?
                .operation(),
            original.operation
        );
        Ok(())
    })
}

#[test]
fn remote_protocol_preserves_binary_data_and_rejects_invalid_envelopes() -> Result<()> {
    let bytes = (0..=255).cycle().take(1_048_576).collect::<Vec<u8>>();
    let encoded = remote::encode(&Response::Data(bytes.clone()))?;
    let Response::Data(decoded) = remote::decode(&encoded)? else {
        anyhow::bail!("Expected binary data");
    };
    assert_eq!(decoded, bytes);
    assert!(remote::decode::<Response>(&encoded[..encoded.len() - 1]).is_err());
    let mut trailing = encoded.clone();
    trailing.push(0);
    assert!(remote::decode::<Response>(&trailing).is_err());
    let mut unsupported = encoded;
    unsupported[0] = 0;
    assert!(remote::decode::<Response>(&unsupported).is_err());
    Ok(())
}

#[test]
fn remote_publication_survives_lost_waiter_is_idempotent_and_recovers_after_reconnect() -> Result<()>
{
    smol::block_on(async {
        let fixture = changed_fixture()?;
        let service = Service::default();
        let repository = open(&service, &fixture).await?;
        assert_ne!(repository, open(&service, &fixture).await?);
        let original = snapshot(&service, repository, true).await?;
        let preparation = revert(&service, &original, 7).await?;
        let Response::Prepared {
            snapshot: prepared,
            receipt: recovery,
            ..
        } = request(&service, preparation.clone()).await?
        else {
            anyhow::bail!("Expected prepared operation");
        };
        let Response::Prepared {
            snapshot: duplicate,
            receipt: repeated_recovery,
            ..
        } = request(&service, preparation).await?
        else {
            anyhow::bail!("Expected retained prepared operation");
        };
        assert_eq!(prepared.operation, duplicate.operation);
        assert_eq!(recovery.data, repeated_recovery.data);
        assert_eq!(
            publication_status(&service, &recovery).await?,
            PublicationStatus::Rejected
        );
        assert_ne!(prepared.operation, original.operation);
        assert_eq!(
            snapshot(&service, repository, false).await?.operation,
            original.operation
        );
        assert_eq!(fs::read(fixture.root().join("file.txt"))?, b"changed\n");
        let bytes: Vec<u8> = read(
            &service,
            &prepared,
            Read::FileContent {
                revision: "@".into(),
                path: "file.txt".into(),
            },
        )
        .await?;
        assert_eq!(bytes, b"base\n");

        // Polling starts publication; dropping this waiter simulates a lost RPC response.
        let first = service
            .execute(Request::Publish { prepared: 7 })
            .now_or_never();
        if let Some(result) = first {
            result?;
        }

        let Response::Published(published) =
            request(&service, Request::Publish { prepared: 7 }).await?
        else {
            anyhow::bail!("Expected retained publication");
        };
        let Response::Published(duplicate) =
            request(&service, Request::Publish { prepared: 7 }).await?
        else {
            anyhow::bail!("Expected repeated publication");
        };
        assert_eq!(published.snapshot.operation, duplicate.snapshot.operation);
        assert_eq!(fs::read(fixture.root().join("file.txt"))?, b"base\n");
        let bytes: Vec<u8> = read(
            &service,
            &original,
            Read::FileContent {
                revision: "@".into(),
                path: "file.txt".into(),
            },
        )
        .await?;
        assert_eq!(bytes, b"changed\n");

        service.close()?;
        let reconnected = Service::default();
        let repository = open(&reconnected, &fixture).await?;
        assert_ne!(repository, original.repository);
        assert!(
            snapshot(&reconnected, original.repository, true)
                .await
                .is_err()
        );
        // This receipt reached the client before Publish, so recovery does not depend on its response.
        let recovery = remote::Receipt {
            repository,
            ..recovery
        };
        assert_eq!(
            publication_status(&reconnected, &recovery).await?,
            PublicationStatus::Applied
        );
        request(&reconnected, Request::Synchronize { receipt: recovery }).await?;
        let receipt = remote::Receipt {
            repository,
            ..published.receipt
        };
        assert_eq!(
            publication_status(&reconnected, &receipt).await?,
            PublicationStatus::Applied
        );
        request(
            &reconnected,
            Request::Synchronize {
                receipt: receipt.clone(),
            },
        )
        .await?;
        fixture.run(&["undo"])?;
        assert_eq!(fs::read(fixture.root().join("file.txt"))?, b"changed\n");
        assert_eq!(
            publication_status(&reconnected, &receipt).await?,
            PublicationStatus::Applied
        );
        reconnected.close()?;
        Ok(())
    })
}

#[test]
fn remote_saved_classification_keeps_capture_retries_and_later_saves_separate() -> Result<()> {
    smol::block_on(async {
        let fixture = changed_fixture()?;
        let service = Service::default();
        let repository = open(&service, &fixture).await?;
        let original = snapshot(&service, repository, true).await?;
        let local = fixture.repository().await?.snapshot(false).await?;
        let classification = ClassificationRequest {
            comparison: local.exact_revision("@-").await?.revset(),
            range_start: None,
            source: None,
            target: ClassificationTarget::Category(Category::Reviewed),
            files: local
                .changed_files(&DiffScope::Change("@".into()))
                .await?
                .into_iter()
                .map(|file| FileSelection { file, ranges: None })
                .collect(),
        };
        let path = fixture.root().join("file.txt");
        fs::write(&path, b"captured\r\n")?;
        let capture = Request::CaptureSaved {
            id: 20,
            repository,
            paths: vec!["file.txt".into()],
        };
        let Response::Saved { version, files, .. } = request(&service, capture.clone()).await?
        else {
            anyhow::bail!("Expected capture");
        };
        assert_eq!(
            files["file.txt"].bytes.as_deref(),
            Some(b"captured\r\n".as_slice())
        );
        fs::write(&path, b"later save\r\n")?;
        service.record_saved_file(&path)?;
        let Response::Saved {
            version: repeated_version,
            files: repeated_files,
            ..
        } = request(&service, capture).await?
        else {
            anyhow::bail!("Expected retained capture");
        };
        assert_eq!(version, repeated_version);
        assert_eq!(files, repeated_files);
        assert_eq!(
            snapshot(&service, repository, true).await?.operation,
            original.operation
        );
        let Response::Saved {
            version: later_version,
            ..
        } = request(
            &service,
            Request::CaptureSaved {
                id: 21,
                repository,
                paths: vec!["file.txt".into()],
            },
        )
        .await?
        else {
            anyhow::bail!("Expected later capture");
        };
        assert_ne!(version, later_version);
        let Response::Prepared { receipt, .. } = request(
            &service,
            Request::Prepare {
                id: 30,
                snapshot: original.clone(),
                mutation: Mutation::Classify {
                    previous: original.clone(),
                    request: classification,
                    saved: vec![20],
                },
            },
        )
        .await?
        else {
            anyhow::bail!("Expected prepared classification");
        };
        assert_eq!(
            publication_status(&service, &receipt).await?,
            PublicationStatus::Rejected
        );
        let Response::Published(published) =
            request(&service, Request::Publish { prepared: 30 }).await?
        else {
            anyhow::bail!("Expected classification");
        };
        assert_eq!(
            publication_status(&service, &receipt).await?,
            PublicationStatus::Applied
        );
        let destination = published
            .destination
            .context("Classification destination")?;
        let bytes: Vec<u8> = read(
            &service,
            &published.snapshot,
            Read::FileContent {
                revision: destination.revset(),
                path: "file.txt".into(),
            },
        )
        .await?;
        assert_eq!(bytes, b"captured\r\n");
        assert_eq!(fs::read(&path)?, b"later save\r\n");
        assert!(matches!(
            request(&service, Request::FinishSaved { capture: 20 }).await?,
            Response::Done
        ));
        let first = service
            .execute(Request::FinishSaved { capture: 21 })
            .now_or_never();
        if let Some(result) = first {
            result?;
        }

        let Response::Snapshot(finished) =
            request(&service, Request::FinishSaved { capture: 21 }).await?
        else {
            anyhow::bail!("Expected final saved snapshot");
        };
        let Response::Snapshot(repeated) =
            request(&service, Request::FinishSaved { capture: 21 }).await?
        else {
            anyhow::bail!("Expected retained final saved snapshot");
        };
        assert_eq!(finished.operation, repeated.operation);
        let bytes: Vec<u8> = read(
            &service,
            &finished,
            Read::FileContent {
                revision: "@".into(),
                path: "file.txt".into(),
            },
        )
        .await?;
        assert_eq!(bytes, b"later save\r\n");
        service.close()?;
        Ok(())
    })
}

#[test]
fn remote_sparse_cancellation_prevents_delayed_checkout() -> Result<()> {
    smol::block_on(async {
        let fixture = changed_fixture()?;
        fixture.run(&["new"])?;
        fixture.run(&["sparse", "set", "--clear", "--add", "visible.txt"])?;
        let service = Service::default();
        let repository = open(&service, &fixture).await?;
        let root = fs::canonicalize(fixture.root())?;
        let guard = crate::transaction::write_lock(&root).await?;
        let mut checkout = Box::pin(request(
            &service,
            Request::ExpandSparseCheckout {
                id: 1,
                repository,
                paths: vec!["file.txt".into()],
            },
        ));
        assert!(futures::poll!(&mut checkout).is_pending());
        request(&service, Request::CancelSparseCheckout { id: 1 }).await?;
        drop(guard);
        assert!(matches!(checkout.await?, Response::SparseCancelled));
        assert!(!fixture.root().join("file.txt").exists());
        request(&service, Request::CancelSparseCheckout { id: 2 }).await?;
        assert!(matches!(
            request(
                &service,
                Request::ExpandSparseCheckout {
                    id: 2,
                    repository,
                    paths: vec!["file.txt".into()],
                }
            )
            .await?,
            Response::SparseCancelled
        ));
        assert!(!fixture.root().join("file.txt").exists());
        assert!(matches!(
            request(
                &service,
                Request::ExpandSparseCheckout {
                    id: 3,
                    repository,
                    paths: vec!["file.txt".into()],
                }
            )
            .await?,
            Response::Snapshot(_)
        ));
        assert_eq!(fs::read(fixture.root().join("file.txt"))?, b"changed\n");
        service.close()?;
        Ok(())
    })
}

#[test]
fn remote_cancellation_and_root_close_release_write_locks_and_preserve_other_roots() -> Result<()> {
    smol::block_on(async {
        let first = changed_fixture()?;
        let second = changed_fixture()?;
        let service = Service::default();
        let first_repository = open(&service, &first).await?;
        let second_repository = open(&service, &second).await?;
        let first_snapshot = snapshot(&service, first_repository, true).await?;
        let second_snapshot = snapshot(&service, second_repository, true).await?;
        request(&service, Request::ReleaseSaved { capture: 90 }).await?;
        assert!(
            request(
                &service,
                Request::CaptureSaved {
                    id: 90,
                    repository: first_repository,
                    paths: vec!["file.txt".into()]
                }
            )
            .await
            .is_err()
        );
        request(&service, Request::ReleasePrepared { prepared: 91 }).await?;
        assert!(
            request(&service, revert(&service, &first_snapshot, 91).await?)
                .await
                .is_err()
        );

        let root = fs::canonicalize(first.root())?;
        let guard = crate::transaction::write_lock(&root).await?;
        let preparation = revert(&service, &first_snapshot, 92).await?;
        let mut preparing = Box::pin(request(&service, preparation));
        assert!(futures::poll!(&mut preparing).is_pending());
        request(&service, Request::ReleasePrepared { prepared: 92 }).await?;
        drop(guard);
        assert!(preparing.await.is_err());
        assert_eq!(
            snapshot(&service, first_repository, false).await?.operation,
            first_snapshot.operation
        );

        request(&service, revert(&service, &first_snapshot, 93).await?).await?;
        request(
            &service,
            Request::DetachRepository {
                repository: first_repository,
            },
        )
        .await?;
        assert!(
            request(&service, Request::Publish { prepared: 93 })
                .await
                .is_err()
        );
        fs::write(first.root().join("file.txt"), b"unrecorded\n")?;
        assert!(snapshot(&service, first_repository, true).await.is_err());
        assert_eq!(
            snapshot(&service, first_repository, false).await?.operation,
            first_snapshot.operation
        );
        let bytes: Vec<u8> = read(
            &service,
            &first_snapshot,
            Read::FileContent {
                revision: "@".into(),
                path: "file.txt".into(),
            },
        )
        .await?;
        assert_eq!(bytes, b"changed\n");
        assert_ne!(open(&service, &first).await?, first_repository);

        request(&service, revert(&service, &second_snapshot, 94).await?).await?;
        request(&service, Request::Publish { prepared: 94 }).await?;
        assert_eq!(fs::read(second.root().join("file.txt"))?, b"base\n");
        assert_eq!(fs::read(first.root().join("file.txt"))?, b"unrecorded\n");
        service.close()?;
        assert!(snapshot(&service, second_repository, false).await.is_err());
        Ok(())
    })
}
