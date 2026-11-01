use super::*;

#[test]
fn split_checkpoints_yield_to_diff_reads_and_release_on_cancel() -> Result<()> {
    use futures::FutureExt as _;
    smol::block_on(async {
        let fixture = Fixture::create()?;
        fs::write(fixture.root().join("file.txt"), "base\n")?;
        let repository = fixture.repository().await?;
        let snapshot = repository
            .snapshot(true)
            .await?
            .with_lifetime(Some(RepositoryLifetime::default()));
        let mut transaction = crate::transaction::Transaction::begin(&snapshot).await?;
        transaction.prioritize_diff_opening();
        let first = snapshot.repository().begin_diff_opening().await?;
        let second = snapshot.repository().begin_diff_opening().await?;
        let mut prepared = Box::pin(transaction.prepare(&["describe", "-m", "prepared only"]));
        assert!(prepared.as_mut().now_or_never().is_none());
        assert_eq!(snapshot.file_content("@", "file.txt").await?, b"base\n");
        drop(first);
        assert!(prepared.as_mut().now_or_never().is_none());
        drop(second);
        prepared.await?;
        assert_eq!(
            repository.snapshot(false).await?.operation(),
            snapshot.operation()
        );
        let opening = snapshot.repository().begin_diff_opening().await?;
        let mut wait = Box::pin(snapshot.repository().yield_to_diff_opening());
        assert!(wait.as_mut().now_or_never().is_none());
        snapshot
            .repository()
            .lifetime()
            .expect("lifetime")
            .invalidate();
        assert!(wait.await.is_err());
        drop(opening);
        let first_lifetime = RepositoryLifetime::default();
        let second_lifetime = first_lifetime.sharing_openings();
        let first = snapshot.with_lifetime(Some(first_lifetime.clone()));
        let second = snapshot.with_lifetime(Some(second_lifetime.clone()));
        let opening = second.repository().begin_diff_opening().await?;
        let mut first_wait = Box::pin(first.repository().yield_to_diff_opening());
        assert!(first_wait.as_mut().now_or_never().is_none());
        first_lifetime.invalidate();
        assert!(
            first_wait.await.is_err(),
            "closing one handle cancels only its waiter"
        );
        let mut second_wait = Box::pin(second.repository().yield_to_diff_opening());
        assert!(
            second_wait.as_mut().now_or_never().is_none(),
            "another handle retains its opening"
        );
        drop(opening);
        second_wait.await?;
        Ok(())
    })
}

#[test]
fn closed_repository_lifetimes_cancel_prepared_and_queued_publications() -> Result<()> {
    smol::block_on(async {
        let fixture = Fixture::create()?;
        fs::write(fixture.root().join("file.txt"), "base\n")?;
        fixture.run(&["new", "-m", "source"])?;
        fs::write(fixture.root().join("file.txt"), "changed\n")?;
        let repository = fixture.repository().await?;
        let original = repository.snapshot(true).await?;
        let scope = DiffScope::Change("@".into());
        let file = selection(&original, scope.clone(), "file.txt", None).await?;
        let lifetime = RepositoryLifetime::default();
        let guarded = original.with_lifetime(Some(lifetime.clone()));
        let prepared = guarded.prepare_file_revert(&scope, &file.file).await?;
        lifetime.invalidate();
        let error = prepared
            .publish()
            .await
            .expect_err("closed repository cannot publish");
        assert!(error.to_string().contains("repository was closed"));
        assert_eq!(
            repository.snapshot(false).await?.operation(),
            original.operation()
        );
        assert_eq!(fs::read(fixture.root().join("file.txt"))?, b"changed\n");
        assert!(
            guarded
                .prepare_file_revert(&scope, &file.file)
                .await
                .is_err()
        );
        assert!(guarded.repository().snapshot_before_write().await.is_err());

        let lifetime = RepositoryLifetime::default();
        let reopened = original.with_lifetime(Some(lifetime.clone()));
        let request = ClassificationRequest {
            comparison: "@-".into(),
            range_start: None,
            source: None,
            target: ClassificationTarget::Category(Category::Reviewed),
            files: vec![file.clone()],
        };
        let prepared = reopened.prepare_classification(&request).await?;
        lifetime.invalidate();
        assert!(prepared.publish().await.is_err());
        assert_eq!(
            repository.snapshot(false).await?.operation(),
            original.operation()
        );
        assert_eq!(fs::read(fixture.root().join("file.txt"))?, b"changed\n");

        let fresh = original.with_lifetime(Some(RepositoryLifetime::default()));
        let publication = fresh
            .prepare_file_revert(&scope, &file.file)
            .await?
            .publish()
            .await?;
        assert_single_publication(&publication).await?;
        assert!(guarded.repository().check_lifetime().is_err());
        assert!(reopened.repository().check_lifetime().is_err());
        assert_eq!(fs::read(fixture.root().join("file.txt"))?, b"base\n");
        Ok(())
    })
}

#[test]
fn saved_verification_waits_for_editor_writes_and_rejects_cancelled_saves() -> Result<()> {
    smol::block_on(async {
        let fixture = Fixture::create()?;
        let path = fixture.root().join("file.txt");
        fs::write(&path, "captured\n")?;
        let repository = fixture.repository().await?;
        let saved = SavedSnapshot::capture(&repository, ["file.txt".into()])?;
        let write = SavedFileWrite::begin(&path)?;
        fs::write(&path, "still writing")?;
        let mut verification = Box::pin(saved.verify());
        assert!(futures::poll!(&mut verification).is_pending());
        fs::write(&path, "complete\r\n")?;
        write.finish()?;
        verification.await?;
        assert_eq!(
            saved.files()?["file.txt"].bytes.as_deref(),
            Some(b"captured\n".as_slice())
        );
        let write = SavedFileWrite::begin(&path)?;
        let mut finishing = Box::pin(saved.finish());
        assert!(futures::poll!(&mut finishing).is_pending());
        fs::write(&path, "final save\n")?;
        write.finish()?;
        let finished = finishing.await?.context("finished saved capture")?;
        assert_eq!(
            finished.file_content("@", "file.txt").await?,
            b"final save\n"
        );
        let saved = SavedSnapshot::capture(&repository, ["file.txt".into()])?;
        let cancelled = SavedFileWrite::begin(&path)?;
        let mut verification = Box::pin(saved.verify());
        assert!(futures::poll!(&mut verification).is_pending());
        drop(cancelled);
        assert!(
            verification
                .await
                .expect_err("cancelled save must reject classification")
                .to_string()
                .contains("cancelled")
        );
        Ok(())
    })
}

#[test]
fn saved_classification_keeps_captured_bytes_and_later_saves_separate() -> Result<()> {
    smol::block_on(async {
        let fixture = Fixture::create()?;
        let path = fixture.root().join("file.txt");
        fs::write(&path, b"base\r\n")?;
        fixture.run(&["new", "-m", "source"])?;
        fs::write(&path, b"recorded\r\n")?;
        let repository = fixture.repository().await?;
        let original = repository.snapshot(true).await?;
        let request = ClassificationRequest {
            comparison: original.exact_revision("@-").await?.revset(),
            range_start: None,
            source: None,
            target: ClassificationTarget::Category(Category::Reviewed),
            files: vec![
                selection(&original, DiffScope::Change("@".into()), "file.txt", None).await?,
            ],
        };
        let captured = b"\xef\xbb\xbfcaptured\r\n";
        fs::write(&path, captured)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
        }
        let saved = SavedSnapshot::capture(&repository, ["file.txt".into()])?;
        assert_eq!(
            repository.snapshot(true).await?.operation(),
            original.operation()
        );
        let prepared = original
            .prepare_classification_with_saved(&original, &request, vec![saved.clone()])
            .await?
            .context("captured classification")?;
        assert_eq!(
            repository.snapshot(false).await?.operation(),
            original.operation()
        );
        assert_eq!(
            prepared.snapshot().file_content("@", "file.txt").await?,
            b"recorded\r\n"
        );
        assert_eq!(
            prepared
                .snapshot()
                .file_content(&prepared.destination().revset(), "file.txt")
                .await?,
            captured
        );
        assert_eq!(fs::read(&path)?, captured);
        let later = b"\xef\xbb\xbflater saved\r\n";
        fs::write(&path, later)?;
        record_saved_file(repository.root(), "file.txt")?;
        let next = SavedSnapshot::capture(&repository, ["file.txt".into()])?;
        assert_ne!(saved.version(), next.version());
        let result = prepared.publish().await?;
        assert_single_publication(&result.publication).await?;
        assert_eq!(
            result.publication.receipt.status().await?,
            PublicationStatus::Applied
        );
        assert_eq!(
            result
                .publication
                .snapshot
                .file_content(&result.destination.revset(), "file.txt")
                .await?,
            captured
        );
        assert_eq!(
            repository
                .snapshot(true)
                .await?
                .file_content("@", "file.txt")
                .await?,
            b"recorded\r\n"
        );
        assert_eq!(fs::read(&path)?, later);
        assert!(saved.finish().await?.is_none());
        let finished = next.finish().await?.context("final saved snapshot")?;
        assert_eq!(finished.file_content("@", "file.txt").await?, later);
        assert_eq!(
            result.publication.receipt.status().await?,
            PublicationStatus::Applied
        );
        assert_eq!(
            finished
                .file_content(&result.destination.revset(), "file.txt")
                .await?,
            captured
        );
        assert_eq!(fs::read(&path)?, later);
        assert!(saved.finish().await?.is_none());
        Ok(())
    })
}

#[test]
fn saved_classification_rejects_unrecorded_writes_before_publication() -> Result<()> {
    smol::block_on(async {
        let fixture = Fixture::create()?;
        let path = fixture.root().join("file.txt");
        fs::write(&path, "base\n")?;
        fixture.run(&["new"])?;
        fs::write(&path, "saved\n")?;
        let repository = fixture.repository().await?;
        let original = repository.snapshot(true).await?;
        let request = ClassificationRequest {
            comparison: original.exact_revision("@-").await?.revset(),
            range_start: None,
            source: None,
            target: ClassificationTarget::Category(Category::Reviewed),
            files: vec![
                selection(&original, DiffScope::Change("@".into()), "file.txt", None).await?,
            ],
        };
        let saved = SavedSnapshot::capture(&repository, ["file.txt".into()])?;
        let prepared = original
            .prepare_classification_with_saved(&original, &request, vec![saved.clone()])
            .await?
            .context("prepared")?;
        fs::write(&path, "external writer\n")?;
        let error = prepared
            .publish()
            .await
            .err()
            .context("must reject changed file")?;
        assert!(error.to_string().contains("outside a recorded save"));
        assert_eq!(
            repository.snapshot(false).await?.operation(),
            original.operation()
        );
        assert_eq!(fs::read(&path)?, b"external writer\n");
        drop(saved);
        assert_eq!(
            repository
                .snapshot(true)
                .await?
                .file_content("@", "file.txt")
                .await?,
            b"external writer\n"
        );
        let snapshot = repository.snapshot(false).await?;
        for path in [".kk/config.toml", "nested/.git/config", r"nested\.jj\repo"] {
            assert!(SavedSnapshot::capture(&repository, [path.into()]).is_err());
            let file = ChangedFile {
                path: path.into(),
                change_type: ChangeKind::Modified,
                renamed_from: None,
                has_conflict: false,
            };
            let error = snapshot
                .prepare_file_revert(&DiffScope::Change("@".into()), &file)
                .await
                .err()
                .context("reject metadata revert")?;
            assert!(error.to_string().contains("Repository metadata"));
            let error = snapshot
                .file_conflict("@", path)
                .await
                .err()
                .context("reject metadata conflict")?;
            assert!(error.to_string().contains("Repository metadata"));
        }

        assert_eq!(
            repository.snapshot(false).await?.operation(),
            snapshot.operation()
        );
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&path, fixture.root().join("link.txt"))?;
            assert!(SavedSnapshot::capture(&repository, ["link.txt".into()]).is_err());
        }
        Ok(())
    })
}

#[test]
fn queued_classification_relocates_old_original_side_and_skips_completed_files() -> Result<()> {
    smol::block_on(async {
        let fixture = Fixture::create()?;
        fs::write(fixture.root().join("file.txt"), "a\nkeep\nb\n")?;
        fixture.run(&["new", "-m", "source"])?;
        fs::write(fixture.root().join("file.txt"), "extra\nA\nkeep\nB\n")?;
        let repository = fixture.repository().await?;
        let original = repository.snapshot(true).await?;
        let base = original.exact_revision("@-").await?;
        let scope = DiffScope::Comparison {
            base: base.revset(),
            target: "@".into(),
        };
        let first = ClassificationRequest {
            comparison: base.revset(),
            range_start: None,
            source: None,
            target: ClassificationTarget::Category(Category::Reviewed),
            files: vec![
                selection(
                    &original,
                    scope.clone(),
                    "file.txt",
                    Some(vec![LineRange {
                        side: SelectionSide::Modified,
                        begin: 1,
                        end: 2,
                    }]),
                )
                .await?,
            ],
        };
        let second = ClassificationRequest {
            target: ClassificationTarget::Category(Category::Rejected),
            files: vec![
                selection(
                    &original,
                    scope.clone(),
                    "file.txt",
                    Some(vec![LineRange {
                        side: SelectionSide::Original,
                        begin: 3,
                        end: 3,
                    }]),
                )
                .await?,
            ],
            ..first.clone()
        };
        let first_result = original
            .prepare_classification(&first)
            .await?
            .publish()
            .await?;
        let snapshot = &first_result.publication.snapshot;
        let rebased = snapshot
            .rebase_classification(&original, &second)
            .await?
            .context("remaining request")?;
        assert_eq!(
            rebased.files[0].ranges,
            Some(vec![LineRange {
                side: SelectionSide::Original,
                begin: 4,
                end: 4
            }])
        );
        let second_result = snapshot
            .prepare_classification(&rebased)
            .await?
            .publish()
            .await?;
        let snapshot = &second_result.publication.snapshot;
        assert_eq!(
            snapshot
                .file_content(&first_result.destination.revset(), "file.txt")
                .await?,
            b"extra\nA\nkeep\nb\n"
        );
        assert_eq!(
            snapshot
                .file_content(&second_result.destination.revset(), "file.txt")
                .await?,
            b"extra\nA\nkeep\nB\n"
        );
        assert!(
            snapshot
                .rebase_classification(&original, &first)
                .await?
                .is_none()
        );
        assert!(
            snapshot
                .rebase_classification(&original, &second)
                .await?
                .is_none()
        );
        assert_eq!(
            fs::read(fixture.root().join("file.txt"))?,
            b"extra\nA\nkeep\nB\n"
        );
        Ok(())
    })
}

async fn selection(
    snapshot: &Snapshot,
    scope: DiffScope,
    path: &str,
    ranges: Option<Vec<LineRange>>,
) -> Result<FileSelection> {
    let file = snapshot
        .changed_files(&scope)
        .await?
        .into_iter()
        .find(|file| file.path == path)
        .context("Missing selected file")?;
    Ok(FileSelection { file, ranges })
}

#[test]
fn saved_classification_preserves_opaque_lfs_and_deleted_versions() -> Result<()> {
    smol::block_on(async {
        use sha2::{Digest as _, Sha256};
        let fixture = Fixture::create()?;
        fs::write(
            fixture.root().join(".gitattributes"),
            "*.bin filter=lfs -text\n",
        )?;
        for path in ["opaque.dat", "large.bin", "delete.txt"] {
            fs::write(fixture.root().join(path), b"base\n")?;
        }

        fixture.run(&["new", "-m", "source"])?;
        for path in ["opaque.dat", "large.bin", "delete.txt"] {
            fs::write(fixture.root().join(path), b"recorded\n")?;
        }

        let repository = fixture.repository().await?;
        let original = repository.snapshot(true).await?;
        let request = ClassificationRequest {
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
        let captured = [0, 255, 0, 128, 13, 10];
        for path in ["opaque.dat", "large.bin"] {
            fs::write(fixture.root().join(path), captured)?;
        }

        fs::remove_file(fixture.root().join("delete.txt"))?;
        let saved = SavedSnapshot::capture(
            &repository,
            ["opaque.dat".into(), "large.bin".into(), "delete.txt".into()],
        )?;
        let prepared = original
            .prepare_classification_with_saved(&original, &request, vec![saved.clone()])
            .await?
            .context("prepared opaque inputs")?;
        let result = prepared.publish().await?;
        let snapshot = &result.publication.snapshot;
        assert_eq!(
            snapshot
                .file_content(&result.destination.revset(), "opaque.dat")
                .await?,
            captured
        );
        let pointer = snapshot
            .file_content(&result.destination.revset(), "large.bin")
            .await?;
        let pointer = LfsPointer::parse(&pointer).context("classified LFS pointer")?;
        assert_eq!(pointer.hash, format!("{:x}", Sha256::digest(captured)));
        assert_eq!(pointer.size, captured.len() as u64);
        assert!(
            snapshot
                .file_content_if_exists(&result.destination.revset(), "delete.txt")
                .await?
                .is_none()
        );
        assert_eq!(
            snapshot.file_content("@", "delete.txt").await?,
            b"recorded\n"
        );
        assert!(!fixture.root().join("delete.txt").exists());
        assert_eq!(fs::read(fixture.root().join("large.bin"))?, captured);
        let final_snapshot = saved.finish().await?.context("final snapshot")?;
        assert!(
            final_snapshot
                .file_content_if_exists("@", "delete.txt")
                .await?
                .is_none()
        );
        assert_eq!(fs::read(fixture.root().join("opaque.dat"))?, captured);
        Ok(())
    })
}

#[test]
fn saved_classification_retains_sparse_files_and_waits_for_a_new_capture() -> Result<()> {
    smol::block_on(async {
        let fixture = Fixture::create()?;
        fs::write(fixture.root().join("visible.txt"), "base\n")?;
        fs::write(fixture.root().join("excluded.txt"), "base\n")?;
        fixture.run(&["new"])?;
        fs::write(fixture.root().join("visible.txt"), "saved\n")?;
        fs::write(fixture.root().join("excluded.txt"), "excluded saved\n")?;
        fixture.run(&["sparse", "set", "--clear", "--add", "visible.txt"])?;
        let repository = fixture.repository().await?;
        let original = repository.snapshot(true).await?;
        assert!(!fixture.root().join("excluded.txt").exists());
        let request = ClassificationRequest {
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
        let saved =
            SavedSnapshot::capture(&repository, ["visible.txt".into(), "excluded.txt".into()])?;
        let result = original
            .prepare_classification_with_saved(&original, &request, vec![saved.clone()])
            .await?
            .context("sparse classification")?
            .publish()
            .await?;
        assert_eq!(
            result
                .publication
                .snapshot
                .file_content(&result.destination.revset(), "excluded.txt")
                .await?,
            b"excluded saved\n"
        );
        assert!(!fixture.root().join("excluded.txt").exists());
        let guard = crate::transaction::write_lock(repository.root()).await?;
        let mut finishing = Box::pin(saved.finish());
        let pending =
            std::future::poll_fn(|cx| std::task::Poll::Ready(finishing.as_mut().poll(cx))).await;
        assert!(pending.is_pending());
        fs::write(fixture.root().join("visible.txt"), "next wave\n")?;
        let next = SavedSnapshot::capture(&repository, ["visible.txt".into()])?;
        drop(guard);
        assert!(finishing.await?.is_none());
        assert_eq!(
            repository
                .snapshot(true)
                .await?
                .file_content("@", "visible.txt")
                .await?,
            b"saved\n"
        );
        let final_snapshot = next.finish().await?.context("final capture")?;
        assert_eq!(
            final_snapshot.file_content("@", "visible.txt").await?,
            b"next wave\n"
        );
        assert!(!fixture.root().join("excluded.txt").exists());
        Ok(())
    })
}

#[test]
fn classification_batches_are_private_and_preserve_history() -> Result<()> {
    smol::block_on(async {
        let fixture = Fixture::create()?;
        fs::write(fixture.root().join("lines.txt"), "first\nsecond\nthird\n")?;
        fs::write(fixture.root().join("opaque.bin"), [0, 255, 10])?;
        fixture.run(&["new", "-m", "source"])?;
        fs::write(fixture.root().join("lines.txt"), "FIRST\nsecond\nTHIRD\n")?;
        fs::write(fixture.root().join("opaque.bin"), [0, 254, 12])?;
        fs::write(
            fixture.root().join("untouched.txt"),
            "keep source history\n",
        )?;
        fixture.run(&["new", "-m", "head"])?;
        fs::write(fixture.root().join("tail.txt"), "head only\n")?;
        let repository = fixture.repository().await?;
        let before = repository.snapshot(true).await?;
        let base = before.exact_revision("@--").await?;
        let head = before.exact_revision("@").await?;
        let source = before.exact_revision("@-").await?;
        let scope = DiffScope::Comparison {
            base: base.revset(),
            target: "@".into(),
        };
        let request = ClassificationRequest {
            comparison: base.revset(),
            range_start: None,
            source: None,
            target: ClassificationTarget::Category(Category::Reviewed),
            files: vec![
                selection(
                    &before,
                    scope.clone(),
                    "lines.txt",
                    Some(vec![LineRange {
                        side: SelectionSide::Modified,
                        begin: 1,
                        end: 1,
                    }]),
                )
                .await?,
                selection(&before, scope, "opaque.bin", None).await?,
            ],
        };
        let prepared = before.prepare_classification(&request).await?;
        assert_eq!(
            repository.snapshot(false).await?.operation(),
            before.operation()
        );
        assert_eq!(
            fs::read(fixture.root().join("lines.txt"))?,
            b"FIRST\nsecond\nTHIRD\n"
        );
        assert_eq!(
            prepared
                .snapshot()
                .file_content(&prepared.destination().revset(), "lines.txt")
                .await?,
            b"FIRST\nsecond\nthird\n"
        );
        assert_eq!(
            prepared
                .snapshot()
                .file_content(&prepared.destination().revset(), "opaque.bin")
                .await?,
            [0, 254, 12]
        );
        let result = prepared.publish().await?;
        assert!(result.publication.synchronization_error.is_none());
        assert_eq!(
            result.publication.receipt.status().await?,
            PublicationStatus::Applied
        );
        let after = &result.publication.snapshot;
        assert_eq!(
            after.exact_revision("@").await?.full_change_id,
            head.full_change_id
        );
        assert_eq!(
            after
                .exact_revision(&source.revset())
                .await?
                .full_description,
            source.full_description
        );
        assert_eq!(
            after.file_content("@", "lines.txt").await?,
            b"FIRST\nsecond\nTHIRD\n"
        );
        let remaining = after
            .changed_files(&DiffScope::Change(source.revset()))
            .await?;
        assert!(remaining.iter().any(|file| file.path == "untouched.txt"));
        assert!(!remaining.iter().any(|file| file.path == "opaque.bin"));
        let categories = after.category_commits(&result.range_start).await?;
        assert_eq!(categories.len(), 1);
        assert_eq!(categories[0].category, Category::Reviewed);
        assert_eq!(categories[0].files.len(), 2);
        Ok(())
    })
}

#[test]
fn line_classification_projects_each_source_on_both_sides() -> Result<()> {
    smol::block_on(async {
        for side in [SelectionSide::Original, SelectionSide::Modified] {
            let fixture = Fixture::create()?;
            fs::write(fixture.root().join("file.txt"), "first\nkeep\nlast\n")?;
            fixture.run(&["new", "-m", "first source"])?;
            fs::write(fixture.root().join("file.txt"), "FIRST\nkeep\nlast\n")?;
            fixture.run(&["new", "-m", "second source"])?;
            fs::write(fixture.root().join("file.txt"), "FIRST\nkeep\nLAST\n")?;
            let repository = fixture.repository().await?;
            let before = repository.snapshot(true).await?;
            let base = before.exact_revision("@--").await?;
            let source = before.exact_revision("@-").await?;
            let head = before.exact_revision("@").await?;
            let request = ClassificationRequest {
                comparison: base.revset(),
                range_start: None,
                source: None,
                target: ClassificationTarget::Category(Category::Reviewed),
                files: vec![
                    selection(
                        &before,
                        DiffScope::Comparison {
                            base: base.revset(),
                            target: head.revset(),
                        },
                        "file.txt",
                        Some(vec![LineRange {
                            side,
                            begin: 3,
                            end: 3,
                        }]),
                    )
                    .await?,
                ],
            };
            let prepared = before.prepare_classification(&request).await?;
            assert_eq!(
                prepared
                    .snapshot()
                    .file_content(&prepared.destination().revset(), "file.txt")
                    .await?,
                b"first\nkeep\nLAST\n"
            );
            assert_eq!(
                prepared
                    .snapshot()
                    .file_content(&source.revset(), "file.txt")
                    .await?,
                b"FIRST\nkeep\nLAST\n"
            );
            assert_eq!(
                prepared
                    .snapshot()
                    .file_content(&head.revset(), "file.txt")
                    .await?,
                b"FIRST\nkeep\nLAST\n"
            );
            let result = prepared.publish().await?;
            assert_single_publication(&result.publication).await?;
            assert_eq!(
                fs::read(fixture.root().join("file.txt"))?,
                b"FIRST\nkeep\nLAST\n"
            );
        }
        Ok(())
    })
}

#[test]
fn whole_file_categories_preserve_deleted_and_replaced_history() -> Result<()> {
    smol::block_on(async {
        for category in [
            Category::Pending,
            Category::Reviewed,
            Category::Rejected,
            Category::Debug,
        ] {
            let fixture = Fixture::create()?;
            fs::write(fixture.root().join("file.txt"), "base\n")?;
            fs::write(fixture.root().join("deleted.txt"), "remove me\n")?;
            fixture.run(&["new", "-m", "intermediate"])?;
            fs::write(fixture.root().join("file.txt"), "intermediate\n")?;
            fixture.run(&["new", "-m", "head"])?;
            fs::write(fixture.root().join("file.txt"), "final\n")?;
            fs::remove_file(fixture.root().join("deleted.txt"))?;
            let repository = fixture.repository().await?;
            let before = repository.snapshot(true).await?;
            let base = before.exact_revision("@--").await?;
            let scope = DiffScope::Comparison {
                base: base.revset(),
                target: "@".into(),
            };
            let request = ClassificationRequest {
                comparison: base.revset(),
                range_start: None,
                source: None,
                target: ClassificationTarget::Category(category),
                files: vec![
                    selection(&before, scope.clone(), "file.txt", None).await?,
                    selection(&before, scope, "deleted.txt", None).await?,
                ],
            };
            let prepared = before.prepare_classification(&request).await?;
            assert_eq!(
                repository.snapshot(false).await?.operation(),
                before.operation()
            );
            assert_eq!(
                prepared
                    .snapshot()
                    .file_content(&prepared.destination().revset(), "file.txt")
                    .await?,
                b"final\n"
            );
            assert!(
                prepared
                    .snapshot()
                    .file_content_if_exists(&prepared.destination().revset(), "deleted.txt")
                    .await?
                    .is_none()
            );
            let result = prepared.publish().await?;
            assert_single_publication(&result.publication).await?;
            assert_eq!(fs::read(fixture.root().join("file.txt"))?, b"final\n");
            assert!(!fixture.root().join("deleted.txt").exists());
            fixture.run(&["undo"])?;
            let undone = repository.snapshot(false).await?;
            assert_eq!(
                undone.exact_revision("@").await?.full_commit_id,
                before.exact_revision("@").await?.full_commit_id
            );
        }
        Ok(())
    })
}

#[test]
fn category_reuse_transfer_and_return_preserve_working_tree() -> Result<()> {
    smol::block_on(async {
        let fixture = Fixture::create()?;
        fs::write(fixture.root().join("file.txt"), "before\n")?;
        fixture.run(&["new"])?;
        fs::write(fixture.root().join("file.txt"), "after\n")?;
        fs::write(fixture.root().join("other.txt"), "other\n")?;
        let repository = fixture.repository().await?;
        let before = repository.snapshot(true).await?;
        let comparison = before.exact_revision("@-").await?.revset();
        let mut request = ClassificationRequest {
            comparison,
            range_start: None,
            source: None,
            target: ClassificationTarget::Category(Category::Reviewed),
            files: vec![selection(&before, DiffScope::Change("@".into()), "file.txt", None).await?],
        };
        let first = before
            .prepare_classification(&request)
            .await?
            .publish()
            .await?;
        request.range_start = Some(first.range_start.full_change_id.clone());
        let snapshot = &first.publication.snapshot;
        let scope = DiffScope::Change("@".into());
        request.files = vec![selection(snapshot, scope, "other.txt", None).await?];
        let second = snapshot
            .prepare_classification(&request)
            .await?
            .publish()
            .await?;
        assert_eq!(
            second.destination.full_change_id,
            first.destination.full_change_id
        );
        request.source = Some(second.destination.clone());
        request.target = ClassificationTarget::Category(Category::Rejected);
        request.files = vec![
            selection(
                &second.publication.snapshot,
                DiffScope::Change(second.destination.revset()),
                "file.txt",
                None,
            )
            .await?,
        ];
        let third = second
            .publication
            .snapshot
            .prepare_classification(&request)
            .await?
            .publish()
            .await?;
        request.source = Some(third.destination.clone());
        request.target = ClassificationTarget::Comparison;
        request.files = vec![
            selection(
                &third.publication.snapshot,
                DiffScope::Change(third.destination.revset()),
                "file.txt",
                None,
            )
            .await?,
        ];
        let returned = third
            .publication
            .snapshot
            .prepare_classification(&request)
            .await?
            .publish()
            .await?;
        assert_eq!(
            returned.destination.full_change_id,
            first.range_start.full_change_id
        );
        assert_eq!(fs::read(fixture.root().join("file.txt"))?, b"after\n");
        let final_files = returned
            .publication
            .snapshot
            .changed_files(&DiffScope::Change("@".into()))
            .await?;
        assert_eq!(final_files.len(), 1);
        assert_eq!(final_files[0].path, "file.txt");
        Ok(())
    })
}

#[test]
fn failed_or_stale_classifications_never_publish_private_changes() -> Result<()> {
    smol::block_on(async {
        let fixture = Fixture::create()?;
        fixture.run(&["new"])?;
        fs::write(fixture.root().join("file.txt"), "selected\n")?;
        let repository = fixture.repository().await?;
        let snapshot = repository.snapshot(true).await?;
        let mut request = ClassificationRequest {
            comparison: snapshot.exact_revision("@-").await?.revset(),
            range_start: None,
            source: None,
            target: ClassificationTarget::Category(Category::Debug),
            files: vec![
                selection(
                    &snapshot,
                    DiffScope::Change("@".into()),
                    "file.txt",
                    Some(vec![LineRange {
                        side: SelectionSide::Modified,
                        begin: 100,
                        end: 100,
                    }]),
                )
                .await?,
            ],
        };
        assert!(snapshot.prepare_classification(&request).await.is_err());
        assert_eq!(
            repository.snapshot(false).await?.operation(),
            snapshot.operation()
        );
        request.files[0].ranges = None;
        let prepared = snapshot.prepare_classification(&request).await?;
        fs::write(fixture.root().join("file.txt"), "new external save\n")?;
        assert!(prepared.publish().await.is_err());
        let current = repository.snapshot(false).await?;
        assert_eq!(
            current.file_content("@", "file.txt").await?,
            b"new external save\n"
        );
        assert!(
            current
                .revisions(Some("description(substring:\"KKW: DEBUG\")"), 10)
                .await?
                .is_empty()
        );
        assert_eq!(
            fs::read(fixture.root().join("file.txt"))?,
            b"new external save\n"
        );
        Ok(())
    })
}

#[test]
fn classification_preserves_renames_modes_and_side_branch_snapshots() -> Result<()> {
    smol::block_on(async {
        let fixture = Fixture::create()?;
        fs::write(fixture.root().join("old name.txt"), "base\n")?;
        fixture.run(&["new", "-m", "rename"])?;
        fs::rename(
            fixture.root().join("old name.txt"),
            fixture.root().join("new name.txt"),
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(
                fixture.root().join("new name.txt"),
                fs::Permissions::from_mode(0o755),
            )?;
        }
        fixture.run(&["new", "-m", "head"])?;
        fs::write(fixture.root().join("unselected.txt"), "keep\n")?;
        fixture.run(&["new", "--no-edit", "-r", "@-", "-m", "side branch"])?;
        let repository = fixture.repository().await?;
        let before = repository.snapshot(true).await?;
        let side = before
            .exact_revision("subject(exact:\"side branch\")")
            .await?;
        let base = before.exact_revision("@--").await?;
        let file = selection(
            &before,
            DiffScope::Comparison {
                base: base.revset(),
                target: "@".into(),
            },
            "new name.txt",
            None,
        )
        .await?;
        assert_eq!(file.file.change_type, ChangeKind::Renamed);
        let request = ClassificationRequest {
            comparison: base.revset(),
            range_start: None,
            source: None,
            target: ClassificationTarget::Category(Category::Reviewed),
            files: vec![file],
        };
        let result = before
            .prepare_classification(&request)
            .await?
            .publish()
            .await?;
        let after = &result.publication.snapshot;
        assert!(
            after
                .changed_files(&DiffScope::Comparison {
                    base: side.full_commit_id.clone(),
                    target: side.revset()
                })
                .await?
                .is_empty()
        );
        assert_eq!(
            after
                .file_content(&result.destination.revset(), "new name.txt")
                .await?,
            b"base\n"
        );
        assert!(
            after
                .file_content_if_exists(&result.destination.revset(), "old name.txt")
                .await?
                .is_none()
        );
        assert_eq!(fs::read(fixture.root().join("new name.txt"))?, b"base\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_ne!(
                fs::metadata(fixture.root().join("new name.txt"))?
                    .permissions()
                    .mode()
                    & 0o111,
                0
            );
        }
        let destination_files = after
            .changed_files(&DiffScope::Change(result.destination.revset()))
            .await?;
        assert_eq!(destination_files.len(), 1);
        assert_eq!(destination_files[0].change_type, ChangeKind::Renamed);
        Ok(())
    })
}

#[test]
fn abandoning_private_work_releases_shared_repository_queue() -> Result<()> {
    smol::block_on(async {
        let fixture = Fixture::create()?;
        fixture.run(&["new"])?;
        fs::write(fixture.root().join("file.txt"), "keep\n")?;
        let repository = fixture.repository().await?;
        let independently_opened = fixture.repository().await?;
        let before = repository.snapshot(true).await?;
        let request = ClassificationRequest {
            comparison: before.exact_revision("@-").await?.revset(),
            range_start: None,
            source: None,
            target: ClassificationTarget::Category(Category::Pending),
            files: vec![selection(&before, DiffScope::Change("@".into()), "file.txt", None).await?],
        };
        let prepared = before.prepare_classification(&request).await?;
        let mut pending_snapshot = Box::pin(independently_opened.snapshot(true));
        assert!(
            smol::future::poll_once(&mut pending_snapshot)
                .await
                .is_none()
        );
        assert_eq!(
            repository.snapshot(false).await?.operation(),
            before.operation()
        );
        drop(prepared);
        assert_eq!(pending_snapshot.await?.operation(), before.operation());
        assert_eq!(fs::read(fixture.root().join("file.txt"))?, b"keep\n");
        Ok(())
    })
}

async fn assert_single_publication(publication: &Publication) -> Result<()> {
    assert!(
        publication.synchronization_error.is_none(),
        "{:?}",
        publication.synchronization_error
    );
    assert_eq!(
        publication.receipt.status().await?,
        PublicationStatus::Applied
    );
    let log = publication
        .snapshot
        .read(&[
            "op".into(),
            "log".into(),
            "--limit".into(),
            "2".into(),
            "--no-graph".into(),
            "-T".into(),
            "id ++ \"\\n\"".into(),
        ])
        .await?;
    let operations: Vec<_> = std::str::from_utf8(&log)?.lines().collect();
    assert_eq!(
        operations.get(1).copied(),
        Some(publication.receipt.previous.operation())
    );
    Ok(())
}

#[test]
fn revert_restores_renames_and_bytes_without_touching_other_files() -> Result<()> {
    smol::block_on(async {
        let fixture = Fixture::create()?;
        fs::write(fixture.root().join("old.txt"), "before\r\n")?;
        fs::write(fixture.root().join("binary"), [0, 255, 1])?;
        fixture.run(&["new"])?;
        fs::rename(
            fixture.root().join("old.txt"),
            fixture.root().join("new.txt"),
        )?;
        fs::write(fixture.root().join("binary"), [0, 255, 2])?;
        fs::write(fixture.root().join("keep.txt"), "keep\n")?;
        let repository = fixture.repository().await?;
        let before = repository.snapshot(true).await?;
        let scope = DiffScope::Change("@".into());
        let rename = selection(&before, scope.clone(), "new.txt", None)
            .await?
            .file;
        assert_eq!(rename.change_type, ChangeKind::Renamed);
        let prepared = before.prepare_file_revert(&scope, &rename).await?;
        assert!(fixture.root().join("new.txt").exists());
        let publication = prepared.publish().await?;
        assert_single_publication(&publication).await?;
        assert!(!fixture.root().join("new.txt").exists());
        assert_eq!(fs::read(fixture.root().join("old.txt"))?, b"before\r\n");
        assert_eq!(fs::read(fixture.root().join("keep.txt"))?, b"keep\n");
        let snapshot = publication.snapshot;
        let binary = selection(&snapshot, scope.clone(), "binary", None)
            .await?
            .file;
        let publication = snapshot
            .prepare_file_revert(&scope, &binary)
            .await?
            .publish()
            .await?;
        assert_single_publication(&publication).await?;
        assert_eq!(fs::read(fixture.root().join("binary"))?, [0, 255, 1]);
        fixture.run(&["undo"])?;
        assert_eq!(fs::read(fixture.root().join("binary"))?, [0, 255, 2]);
        assert!(fixture.root().join("old.txt").exists());
        Ok(())
    })
}

#[test]
fn move_to_child_preserves_cancelled_content_and_is_one_undo() -> Result<()> {
    smol::block_on(async {
        let fixture = Fixture::create()?;
        fs::write(fixture.root().join("file.txt"), "original\n")?;
        fixture.run(&["new", "-m", "source"])?;
        fs::write(fixture.root().join("file.txt"), "changed\n")?;
        fs::write(fixture.root().join("stay.txt"), "stay\n")?;
        fixture.run(&["new", "-m", "destination"])?;
        fs::write(fixture.root().join("file.txt"), "original\n")?;
        fixture.run(&["new", "-m", "head"])?;
        let repository = fixture.repository().await?;
        let before = repository.snapshot(true).await?;
        let source = before.exact_revision("@--").await?;
        let destination = before.exact_revision("@-").await?;
        let file = selection(
            &before,
            DiffScope::Change(source.revset()),
            "file.txt",
            None,
        )
        .await?
        .file;
        let publication = before
            .prepare_file_move(&source, FileMoveDirection::Child, &file)
            .await?
            .publish()
            .await?;
        assert_single_publication(&publication).await?;
        let after = &publication.snapshot;
        assert_eq!(
            after.file_content(&source.revset(), "file.txt").await?,
            b"original\n"
        );
        assert!(
            after
                .changed_files(&DiffScope::Change(destination.revset()))
                .await?
                .is_empty()
        );
        assert_eq!(fs::read(fixture.root().join("file.txt"))?, b"original\n");
        fixture.run(&["undo"])?;
        let undone = repository.snapshot(false).await?;
        assert_eq!(
            undone
                .exact_revision(&source.revset())
                .await?
                .full_commit_id,
            source.full_commit_id
        );
        assert_eq!(
            undone
                .exact_revision(&destination.revset())
                .await?
                .full_commit_id,
            destination.full_commit_id
        );
        Ok(())
    })
}

#[test]
fn move_to_parent_preserves_destination_message_and_abandons_empty_source() -> Result<()> {
    smol::block_on(async {
        let fixture = Fixture::create()?;
        fixture.run(&["describe", "-m", "destination"])?;
        fixture.run(&["new", "-m", "source"])?;
        fs::write(fixture.root().join("file.txt"), "move\n")?;
        fixture.run(&["new", "-m", "head"])?;
        let repository = fixture.repository().await?;
        let before = repository.snapshot(true).await?;
        let source = before.exact_revision("@-").await?;
        let destination = before.exact_revision("@--").await?;
        let file = selection(
            &before,
            DiffScope::Change(source.revset()),
            "file.txt",
            None,
        )
        .await?
        .file;
        let publication = before
            .prepare_file_move(&source, FileMoveDirection::Parent, &file)
            .await?
            .publish()
            .await?;
        assert_single_publication(&publication).await?;
        let after = &publication.snapshot;
        assert_eq!(
            after
                .file_content(&destination.revset(), "file.txt")
                .await?,
            b"move\n"
        );
        assert_eq!(
            after
                .exact_revision(&destination.revset())
                .await?
                .full_description,
            destination.full_description
        );
        assert!(
            after
                .revisions(Some(&format!("present({})", source.revset())), 1)
                .await?
                .is_empty()
        );
        assert_eq!(fs::read(fixture.root().join("file.txt"))?, b"move\n");
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
        Ok(())
    })
}
