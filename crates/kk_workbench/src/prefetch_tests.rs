use super::*;
use crate::remote::Read;

#[test]
fn prefetch_bounds_content_and_primes_foreground_queries() -> Result<()> {
    smol::block_on(async {
        let fixture = Fixture::create()?;
        fixture.run(&["config", "set", "--repo", "snapshot.max-new-file-size", "0"])?;
        let pointer = LfsPointer {
            hash: "a".repeat(64),
            size: 4_000_000_000,
        }
        .text();
        for (path, bytes) in [
            ("text.txt", "你好\n".as_bytes()),
            ("--help", "literal path\n".as_bytes()),
            ("binary.bin", b"before\0after".as_slice()),
            ("encoding.bin", b"\xff\xfe".as_slice()),
            ("pointer.txt", pointer.as_bytes()),
        ] {
            fs::write(fixture.root().join(path), bytes)?;
        }
        fs::write(
            fixture.root().join("large.txt"),
            vec![b'x'; PREFETCH_FILE_BYTES + 1],
        )?;
        fs::write(
            fixture.root().join("limit.txt"),
            vec![b'x'; PREFETCH_FILE_BYTES],
        )?;
        fs::write(fixture.root().join("empty.txt"), [])?;
        #[cfg(unix)]
        std::os::unix::fs::symlink("text.txt", fixture.root().join("link.txt"))?;
        let repository = fixture.repository().await?;
        let snapshot = repository.snapshot(true).await?;
        let revision = snapshot.exact_revision("@").await?.full_commit_id;
        assert_eq!(
            snapshot
                .prefetch_file(&revision, "limit.txt")
                .await?
                .contents
                .as_ref()
                .map(Vec::len),
            Some(PREFETCH_FILE_BYTES)
        );
        assert_eq!(
            snapshot
                .prefetch_file(&revision, "empty.txt")
                .await?
                .contents,
            Some(Vec::new())
        );
        #[cfg(unix)]
        {
            let link = snapshot.prefetch_file(&revision, "link.txt").await?;
            assert_eq!(link.information.file_type.as_deref(), Some("symlink"));
            assert!(link.contents.is_none());
        }
        assert_eq!(
            snapshot.prefetch_file(&revision, "--help").await?.contents,
            Some(b"literal path\n".to_vec())
        );
        let text = snapshot.prefetch_file(&revision, "text.txt").await?;
        assert!(text.is_text());
        assert_eq!(text.contents.as_deref(), Some("你好\n".as_bytes()));
        assert_eq!(
            snapshot.cached::<Vec<u8>>(&Read::FileContent {
                revision: revision.clone(),
                path: "text.txt".into(),
            }),
            text.contents
        );
        assert_eq!(
            snapshot.cached::<Option<Vec<u8>>>(&Read::FileContentIfExists {
                revision: revision.clone(),
                path: "text.txt".into(),
            }),
            Some(text.contents.clone())
        );
        assert!(
            snapshot
                .cached::<FileInformation>(&Read::FileInformation {
                    revision: revision.clone(),
                    path: "text.txt".into(),
                })
                .is_some()
        );
        for path in ["binary.bin", "encoding.bin", "large.txt"] {
            let probe = snapshot.prefetch_file(&revision, path).await?;
            assert!(!probe.is_text(), "{path}");
            assert!(probe.contents.is_none(), "{path}");
            assert!(probe.information.size.is_some(), "{path}");
            assert!(
                snapshot
                    .cached::<Vec<u8>>(&Read::FileContent {
                        revision: revision.clone(),
                        path: path.into(),
                    })
                    .is_none(),
                "{path}"
            );
        }
        let probe = snapshot.prefetch_file(&revision, "pointer.txt").await?;
        assert!(!probe.is_text());
        assert_eq!(probe.information.size, Some(4_000_000_000));
        assert!(probe.contents.is_none());
        let missing = snapshot.prefetch_file(&revision, "missing.txt").await?;
        assert_eq!(missing.information.exists, Some(false));
        assert_eq!(
            snapshot.cached::<Option<Vec<u8>>>(&Read::FileContentIfExists {
                revision: revision.clone(),
                path: "missing.txt".into(),
            }),
            Some(None)
        );
        assert_eq!(
            snapshot.file_content(&revision, "text.txt").await?,
            "你好\n".as_bytes()
        );
        assert_eq!(
            snapshot.file_content(&revision, "large.txt").await?.len(),
            PREFETCH_FILE_BYTES + 1
        );
        Ok(())
    })
}

#[test]
fn prefetch_does_not_materialize_sparse_files_and_survives_operation_refresh() -> Result<()> {
    smol::block_on(async {
        let fixture = Fixture::create()?;
        fs::write(fixture.root().join("hidden.txt"), "hidden\n")?;
        fixture.run(&["new"])?;
        fixture.run(&["sparse", "set", "--clear"])?;
        let repository = fixture.repository().await?;
        let snapshot = repository.snapshot(false).await?;
        let revision = snapshot.exact_revision("@").await?.full_commit_id;
        snapshot.prefetch_file(&revision, "hidden.txt").await?;
        assert!(!fixture.root().join("hidden.txt").exists());
        fixture.run(&["describe", "-m", "refresh"])?;
        let refreshed = repository.snapshot(false).await?;
        assert_ne!(snapshot.operation(), refreshed.operation());
        assert_eq!(
            refreshed.cached::<Vec<u8>>(&Read::FileContent {
                revision,
                path: "hidden.txt".into(),
            }),
            Some(b"hidden\n".to_vec())
        );
        Ok(())
    })
}
