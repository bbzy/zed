use super::*;
use anyhow::{Context as _, Result, ensure};
use smol::process::Command;
use std::{fs, path::Path};
use tempfile::TempDir;

#[path = "conflict_tests.rs"]
mod conflict_tests;
#[path = "remote_tests.rs"]
mod remote_tests;
#[path = "transaction_tests.rs"]
mod transaction_tests;

struct Fixture {
    directory: TempDir,
}

impl Fixture {
    fn create() -> Result<Self> {
        Self::from_directory(tempfile::tempdir()?)
    }

    fn from_directory(directory: TempDir) -> Result<Self> {
        let fixture = Self { directory };
        fixture.run(&[
            "init",
            ".",
            "--config",
            "user.name=\"KK Workbench Test\"",
            "--config",
            "user.email=\"kk-workbench@example.invalid\"",
        ])?;
        fixture.run(&["config", "set", "--repo", "user.name", "KK Workbench Test"])?;
        fixture.run(&[
            "config",
            "set",
            "--repo",
            "user.email",
            "kk-workbench@example.invalid",
        ])?;

        Ok(fixture)
    }

    fn root(&self) -> &Path {
        self.directory.path()
    }

    fn run(&self, arguments: &[&str]) -> Result<String> {
        let output = smol::block_on(
            Command::new("kk")
                .current_dir(self.root())
                .args(arguments)
                .args(["--no-pager", "--color", "never"])
                .output(),
        )
        .context("kk must be available for repository integration tests")?;
        ensure!(
            output.status.success(),
            "kk {arguments:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        Ok(String::from_utf8(output.stdout)?)
    }

    async fn repository(&self) -> Result<Repository> {
        Repository::open(self.root(), Path::new("kk")).await
    }
}

#[test]
fn sparse_open_preparation_only_materializes_confirmed_tracked_files() -> Result<()> {
    smol::block_on(async {
        let fixture = Fixture::create()?;
        fs::create_dir(fixture.root().join("hidden"))?;
        for (path, text) in [
            ("visible.txt", "visible\n"),
            ("hidden/selected.txt", "selected\n"),
            ("hidden/other.txt", "other\n"),
            ("-literal[1].txt", "literal\n"),
        ] {
            fs::write(fixture.root().join(path), text)?;
        }

        fixture.run(&["new"])?;
        fixture.run(&["sparse", "set", "--clear", "--add", "visible.txt"])?;
        let repository = fixture.repository().await?;
        let before = repository.snapshot(false).await?;
        let requested = [
            "hidden/selected.txt",
            "visible.txt",
            "hidden/selected.txt",
            "-literal[1].txt",
            "not-tracked.txt",
        ]
        .map(str::to_owned);
        let excluded = repository.excluded_sparse_files(&requested).await?;
        assert_eq!(excluded, ["-literal[1].txt", "hidden/selected.txt"]);
        assert!(
            repository
                .excluded_sparse_files(&["../outside".into()])
                .await
                .is_err()
        );
        assert!(matches!(
            repository
                .expand_sparse_checkout(&excluded, || false)
                .await?,
            SparseCheckout::Cancelled
        ));
        assert!(!fixture.root().join("hidden/selected.txt").exists());
        let saved = SavedSnapshot::capture(&repository, ["visible.txt".into()])?;
        assert!(matches!(
            repository
                .expand_sparse_checkout(&excluded, || true)
                .await?,
            SparseCheckout::PendingSavedInputs
        ));
        assert!(!fixture.root().join("hidden/selected.txt").exists());
        saved.finish().await?;
        let SparseCheckout::Ready(after) = repository
            .expand_sparse_checkout(&excluded, || true)
            .await?
        else {
            anyhow::bail!("expected expanded snapshot");
        };
        assert_eq!(after.operation(), before.operation());
        assert_eq!(
            fs::read_to_string(fixture.root().join("hidden/selected.txt"))?,
            "selected\n"
        );
        assert_eq!(
            fs::read_to_string(fixture.root().join("-literal[1].txt"))?,
            "literal\n"
        );
        assert!(!fixture.root().join("hidden/other.txt").exists());
        assert_eq!(
            fs::read_to_string(fixture.root().join("visible.txt"))?,
            "visible\n"
        );
        assert!(
            repository
                .excluded_sparse_files(&requested)
                .await?
                .is_empty()
        );
        fs::remove_file(fixture.root().join("visible.txt"))?;
        assert!(
            repository
                .excluded_sparse_files(&["visible.txt".into()])
                .await?
                .is_empty()
        );
        let lifetime = RepositoryLifetime::default();
        let closed = after.with_lifetime(Some(lifetime.clone()));
        lifetime.invalidate();
        assert!(
            closed
                .repository()
                .expand_sparse_checkout(&["hidden/other.txt".into()], || true)
                .await
                .is_err()
        );
        assert!(!fixture.root().join("hidden/other.txt").exists());
        Ok(())
    })
}

#[test]
fn diagnostic_commands_report_failures_without_logging_file_contents() -> Result<()> {
    smol::block_on(async {
        let fixture = Fixture::create()?;
        let secret = "PRIVATE FILE CONTENT MUST NOT APPEAR IN OUTPUT";
        fs::write(fixture.root().join("secret.txt"), secret)?;
        let diagnostics = Diagnostics::default();
        diagnostics.configure(&["perf".into()]);
        let repository = Repository::open_with_diagnostics(
            fixture.root(),
            Path::new("kk"),
            diagnostics.clone(),
            None,
        )
        .await?;
        let snapshot = repository.snapshot(true).await?;
        assert_eq!(
            snapshot.file_content("@", "secret.txt").await?,
            secret.as_bytes()
        );
        assert!(
            snapshot
                .revisions(Some("absent_revision_for_diagnostics"), 1)
                .await
                .is_err()
        );
        assert!(
            Repository::open_with_diagnostics(
                fixture.root(),
                Path::new("/nonexistent/kk-diagnostic-test"),
                diagnostics.clone(),
                None
            )
            .await
            .is_err()
        );
        let text = diagnostics.read_since(0).1;
        assert!(!text.contains(secret));
        assert!(text.contains("[COMMAND]") && text.contains("[COMMAND END]"));
        assert!(text.contains("[ERROR]") && text.contains("Failed to launch"));
        assert!(
            text.contains("\"outcome\":\"error\"") && text.contains("\"outcome\":\"completed\"")
        );
        assert!(text.contains("\"stdoutBytes\"") && text.contains("\"elapsedMs\""));
        Ok(())
    })
}

#[test]
fn history_preserves_metadata_and_snapshot_identity() -> Result<()> {
    smol::block_on(async {
        let fixture = Fixture::create()?;
        fixture.run(&["describe", "-m", "subject | \"quoted\"\nsecond line"])?;
        fs::write(fixture.root().join("test.txt"), "before\n")?;
        let repository = fixture.repository().await?;
        let snapshot = repository.snapshot(true).await?;
        let history = snapshot.history(HistoryMode::Lineage, 50).await?;
        let current = history.revisions.first().context("working copy missing")?;
        assert!(current.is_working_copy);
        assert_eq!(current.subject(), "subject | \"quoted\"");
        assert_eq!(
            current.full_description,
            "subject | \"quoted\"\nsecond line\n"
        );
        assert_eq!(current.author_name, "KK Workbench Test");
        assert_eq!(current.parent_commit_ids.len(), 1);
        assert!(!history.has_more);
        assert!(history.revisions.last().context("root missing")?.is_root);
        let emptiness = snapshot
            .commit_emptiness(
                &history
                    .revisions
                    .iter()
                    .map(|revision| revision.full_commit_id.clone())
                    .collect::<Vec<_>>(),
            )
            .await?;
        assert_eq!(emptiness.get(&current.full_commit_id), Some(&false));
        assert_eq!(
            emptiness.get(
                &history
                    .revisions
                    .last()
                    .context("root missing")?
                    .full_commit_id
            ),
            Some(&true)
        );
        assert!(snapshot.commit_emptiness(&[]).await?.is_empty());
        assert!(
            !snapshot
                .comparison_candidates()
                .await?
                .iter()
                .any(|revision| revision.is_working_copy)
        );

        fs::write(fixture.root().join("test.txt"), "after\n")?;
        let refreshed = repository.snapshot(true).await?;
        assert_ne!(snapshot.operation(), refreshed.operation());
        assert_eq!(snapshot.file_content("@", "test.txt").await?, b"before\n");
        assert_eq!(refreshed.file_content("@", "test.txt").await?, b"after\n");
        let refreshed_history = refreshed.history(HistoryMode::Lineage, 50).await?;
        assert_eq!(
            current.full_change_id,
            refreshed_history.revisions[0].full_change_id
        );
        assert_ne!(
            current.full_commit_id,
            refreshed_history.revisions[0].full_commit_id
        );
        assert_eq!(
            refreshed.resolve_revision(current).await?.full_commit_id,
            refreshed_history.revisions[0].full_commit_id,
        );

        Ok(())
    })
}

#[test]
fn exact_paths_and_opaque_contents_do_not_use_shell_or_fileset_syntax() -> Result<()> {
    smol::block_on(async {
        let fixture = Fixture::create()?;
        let paths = [
            "a => {b}.txt",
            "quote\".txt",
            "glob:*.txt",
            "--help",
            "日本語.txt",
        ];
        for path in paths {
            fs::write(fixture.root().join(path), b"\0\xffbinary\r\n")?;
        }

        let repository = fixture.repository().await?;
        let snapshot = repository.snapshot(true).await?;
        let scope = DiffScope::Change("@".into());
        let files = snapshot.changed_files(&scope).await?;
        assert_eq!(files.len(), paths.len());
        for path in paths {
            assert!(
                files
                    .iter()
                    .any(|file| file.path == path && file.change_type == ChangeKind::Added)
            );
            assert_eq!(snapshot.file_content("@", path).await?, b"\0\xffbinary\r\n");
            assert!(!snapshot.diff(&scope, Some(path)).await?.is_empty());
        }

        assert!(snapshot.file_content("@", "../outside").await.is_err());
        assert!(snapshot.diff(&scope, Some("/outside")).await.is_err());
        assert!(snapshot.file_content("@", "missing").await.is_err());
        assert_eq!(snapshot.file_content_if_exists("@", "missing").await?, None);
        assert_eq!(
            snapshot.file_content_if_exists("@", "--help").await?,
            Some(b"\0\xffbinary\r\n".to_vec())
        );
        assert!(
            snapshot
                .file_content_if_exists("nonexistent()", "missing")
                .await
                .is_err()
        );
        assert!(
            snapshot
                .file_content_if_exists("@", "../outside")
                .await
                .is_err()
        );

        Ok(())
    })
}

#[test]
fn file_information_preserves_binary_hashes_missing_files_and_lfs_identity() -> Result<()> {
    smol::block_on(async {
        let fixture = Fixture::create()?;
        let path = "binary\n\".dat";
        fs::write(fixture.root().join(path), b"\0\xff\r\nbinary")?;
        let pointer = "version https://git-lfs.github.com/spec/v1\noid sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad\nsize 3\n";
        fs::write(fixture.root().join("large.bin"), pointer)?;
        let repository = fixture.repository().await?;
        let snapshot = repository.snapshot(true).await?;
        let historical = snapshot.file_information("@", path).await?;
        let native = repository.working_file_information(path, None).await?;
        assert_eq!(historical.path, path);
        assert_eq!(historical.exists, Some(true));
        assert_eq!(historical.hash, native.hash);
        assert_eq!(historical.size, Some(10));
        assert!(historical.object_id.is_some());
        assert!(native.modified.is_some());
        let missing = snapshot.file_information("@", "missing.bin").await?;
        assert_eq!(missing.exists, Some(false));
        assert_eq!(missing.hash, None);
        assert_eq!(missing.pointer_text(), Some(String::new()));
        let lfs = snapshot.file_information("@", "large.bin").await?;
        assert_eq!(lfs.pointer_text().as_deref(), Some(pointer));
        assert_eq!(lfs.size, Some(3));
        let lfs_pointer = lfs.pointer.as_ref().context("LFS pointer")?;
        assert!(repository.lfs_content(lfs_pointer).await.is_err());
        let directory = fixture.root().join(".kk/repo/store/git/lfs/objects/ba/78");
        fs::create_dir_all(&directory)?;
        let object = directory.join(&lfs_pointer.hash);
        fs::write(&object, b"abc")?;
        assert_eq!(repository.lfs_content(lfs_pointer).await?, b"abc");
        fs::write(&object, b"bad")?;
        assert!(repository.lfs_content(lfs_pointer).await.is_err());
        let invalid = LfsPointer {
            hash: "../outside".into(),
            size: 3,
        };
        assert!(repository.lfs_content(&invalid).await.is_err());
        let draft = repository
            .working_file_information("large.bin", Some(b"abc"))
            .await?;
        assert_eq!(lfs.hash, draft.hash);
        assert_eq!(draft.modified.as_deref(), Some("Unsaved editor changes"));
        assert!(LfsPointer::parse(pointer.replace("size 3", "size +3").as_bytes()).is_none());
        assert!(
            LfsPointer::parse(
                pointer
                    .replace("size 3", "size 9007199254740992")
                    .as_bytes()
            )
            .is_none()
        );
        assert!(snapshot.file_information("@", "../outside").await.is_err());
        Ok(())
    })
}

#[test]
fn annotations_keep_line_origins_and_fixed_revision_with_literal_paths() -> Result<()> {
    smol::block_on(async {
        let fixture = Fixture::create()?;
        let path = "--lines\n日本語.txt";
        fixture.run(&["describe", "-m", "first source"])?;
        fs::write(fixture.root().join(path), "first\nsecond\n")?;
        fixture.run(&["new", "-m", "second source"])?;
        fs::write(fixture.root().join(path), "first\nsecond\nthird\n")?;
        let repository = fixture.repository().await?;
        let snapshot = repository.snapshot(true).await?;
        let lines = snapshot.annotate("@", path).await?;
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].summary, "first source");
        assert_eq!(lines[1].full_change_id, lines[0].full_change_id);
        assert_eq!(lines[1].commit_id, lines[0].commit_id);
        assert_eq!(lines[2].summary, "second source");
        assert_ne!(lines[0].commit_id, lines[2].commit_id);
        assert_eq!(
            lines.iter().map(|line| line.first).collect::<Vec<_>>(),
            vec![true, false, true]
        );
        assert!(lines.iter().all(|line| {
            line.author == "KK Workbench Test"
                && line.change_id.len() >= 8
                && !line.change_id_prefix.is_empty()
                && line.change_id.starts_with(&line.change_id_prefix)
                && !line.date.is_empty()
        }));
        for line in &lines {
            let expected = fixture.run(&[
                "log",
                "--no-graph",
                "-r",
                &line.commit_id,
                "-T",
                "change_id.shortest(8).prefix()",
            ])?;
            assert_eq!(line.change_id_prefix, expected);
        }
        fs::write(fixture.root().join(path), "new contents\n")?;
        repository.snapshot(true).await?;
        assert_eq!(snapshot.annotate("@", path).await?, lines);
        assert!(snapshot.annotate("@", "../outside").await.is_err());
        assert!(snapshot.annotate("@", "missing").await.is_err());
        Ok(())
    })
}

#[test]
fn subdirectories_resolve_to_the_same_repository_without_snapshotting() -> Result<()> {
    smol::block_on(async {
        let fixture = Fixture::create()?;
        let directory = fixture.root().join("nested");
        fs::create_dir(&directory)?;
        let repository = fixture.repository().await?;
        let before = repository.snapshot(false).await?;
        fs::write(directory.join("unsnapshotted.txt"), "pending")?;
        assert_eq!(
            repository
                .relative_file_path(&directory.join("unsnapshotted.txt"))
                .await?,
            "nested/unsnapshotted.txt"
        );
        let outside = tempfile::tempdir()?;
        assert!(
            repository
                .relative_file_path(&outside.path().join("outside.txt"))
                .await
                .is_err()
        );
        let nested = Repository::open(&directory, Path::new("kk")).await?;
        assert_eq!(nested.root(), repository.root());
        let after = nested.snapshot(false).await?;
        assert_eq!(before.operation(), after.operation());
        assert!(
            after
                .changed_files(&DiffScope::Change("@".into()))
                .await?
                .is_empty()
        );

        Ok(())
    })
}

#[test]
fn graph_keeps_divergent_commits_and_merge_lanes_distinct() -> Result<()> {
    smol::block_on(async {
        let fixture = Fixture::create()?;
        fs::write(fixture.root().join("base.txt"), "base")?;
        fixture.run(&["new", "-m", "left"])?;
        fs::write(fixture.root().join("left.txt"), "left")?;
        let left = fixture.run(&["log", "-r", "@", "--no-graph", "-T", "change_id"])?;
        fixture.run(&["new", "@-", "-m", "right"])?;
        fs::write(fixture.root().join("right.txt"), "right")?;
        fixture.run(&["new", "@", &left, "-m", "merge"])?;
        let snapshot = fixture.repository().await?.snapshot(true).await?;
        let revisions = snapshot.revisions(Some("::@"), 50).await?;
        let rows = history_graph(&revisions, HistoryMode::Log);
        assert_eq!(rows.len(), revisions.len());
        assert!(rows[0].incoming.is_empty());
        assert!(rows[0].is_merge);
        assert_eq!(rows[0].parents.len(), 2);
        assert_ne!(rows[0].parents[0], rows[0].parents[1]);
        for adjacent in rows.windows(2) {
            assert_eq!(adjacent[0].outgoing, adjacent[1].incoming);
        }
        assert!(rows[0].prefix.contains('─'));
        assert!(rows.iter().any(|row| row.is_fork));
        assert!(rows.iter().all(|row| row.omitted_parents == 0));
        let truncated = history_graph(&revisions[..1], HistoryMode::Log);
        assert_eq!(truncated[0].omitted_parents, 2);
        assert!(truncated[0].parents.is_empty());
        assert!(truncated[0].outgoing.is_empty());
        let disconnected = revisions
            .iter()
            .cloned()
            .map(|mut revision| {
                revision.parent_commit_ids.clear();
                revision
            })
            .collect::<Vec<_>>();
        assert!(
            history_graph(&disconnected, HistoryMode::Log)
                .iter()
                .all(|row| row.incoming.is_empty() && row.outgoing.is_empty())
        );
        assert!(truncated[0].prefix.ends_with(" ~"));
        let lineage = snapshot.history(HistoryMode::Lineage, 50).await?;
        assert!(
            !lineage
                .revisions
                .iter()
                .any(|revision| revision.full_change_id == left)
        );
        let left_revision = revisions
            .iter()
            .find(|revision| revision.full_change_id == left)
            .context("left revision missing")?;
        let (located_history, outside) = snapshot
            .history_containing(HistoryMode::Lineage, 50, left_revision)
            .await?;
        assert_eq!(located_history.revisions.len(), lineage.revisions.len());
        assert_eq!(
            outside
                .context("merge parent must remain outside the first-parent history")?
                .full_commit_id,
            left_revision.full_commit_id
        );

        Ok(())
    })
}

#[test]
fn comparison_boundary_skips_only_exact_category_markers_and_survives_rewrites() -> Result<()> {
    smol::block_on(async {
        let fixture = Fixture::create()?;
        fs::write(fixture.root().join("base.txt"), "base")?;
        fixture.run(&["new", "-m", "KKW: REVIEWED"])?;
        fs::write(fixture.root().join("reviewed.txt"), "reviewed")?;
        fixture.run(&["new", "-m", "KKW: DEBUG"])?;
        fs::write(fixture.root().join("debug.txt"), "debug")?;
        fixture.run(&["new", "-m", "KKW: COMPARISON BASE"])?;
        fixture.run(&["new", "-m", "pending"])?;
        fs::write(fixture.root().join("pending.txt"), "pending")?;
        let repository = fixture.repository().await?;
        let snapshot = repository.snapshot(true).await?;
        let boundary = snapshot.comparison_boundary("@----", None).await?;
        let range_start = boundary.range_start.context("Missing separator")?;
        assert_eq!(range_start.subject(), "KKW: COMPARISON BASE");
        let files = snapshot
            .changed_files(&DiffScope::Comparison {
                base: boundary.base_revset,
                target: "@".into(),
            })
            .await?;
        assert_eq!(
            files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            vec!["pending.txt"]
        );

        fixture.run(&["describe", "-r", "@--", "-m", "rewritten debug"])?;
        let rewritten = repository.snapshot(false).await?;
        let boundary = rewritten
            .comparison_boundary("@----", Some(&range_start.full_change_id))
            .await?;
        assert_eq!(
            boundary
                .range_start
                .context("Missing retained range")?
                .full_change_id,
            range_start.full_change_id
        );
        assert!(
            rewritten
                .comparison_boundary("@----", Some("not valid"))
                .await
                .is_err()
        );

        Ok(())
    })
}

#[test]
fn history_pagination_loads_one_extra_revision_without_losing_order() -> Result<()> {
    smol::block_on(async {
        let fixture = Fixture::create()?;
        for index in 0..51 {
            fixture.run(&["new", "-m", &format!("revision {index}")])?;
        }

        let snapshot = fixture.repository().await?.snapshot(false).await?;
        let first = snapshot.history(HistoryMode::Lineage, 50).await?;
        assert_eq!(first.revisions.len(), 50);
        assert!(first.has_more);
        let second = snapshot.history(HistoryMode::Lineage, 100).await?;
        assert_eq!(second.revisions.len(), 53);
        for (first, second) in first.revisions.iter().zip(&second.revisions) {
            assert!(!first.change_id_prefix.is_empty());
            assert_eq!(first.change_id_prefix, second.change_id_prefix);
        }
        assert!(!second.has_more);
        let target = second.revisions.last().context("root missing")?;
        let (located_history, outside) = snapshot
            .history_containing(HistoryMode::Lineage, 50, target)
            .await?;
        assert_eq!(located_history.revisions.len(), 53);
        assert!(outside.is_none());
        assert_eq!(
            first
                .revisions
                .iter()
                .map(|revision| &revision.full_commit_id)
                .collect::<Vec<_>>(),
            second
                .revisions
                .iter()
                .take(50)
                .map(|revision| &revision.full_commit_id)
                .collect::<Vec<_>>()
        );

        Ok(())
    })
}

#[test]
fn diff_summary_preserves_rename_endpoints() -> Result<()> {
    smol::block_on(async {
        let fixture = Fixture::create()?;
        fs::write(fixture.root().join("old {name}.txt"), "rename me\n")?;
        fixture.run(&["new"])?;
        fs::rename(
            fixture.root().join("old {name}.txt"),
            fixture.root().join("new => name.txt"),
        )?;
        let snapshot = fixture.repository().await?.snapshot(true).await?;
        assert_eq!(
            snapshot.current_file_path("@-", "old {name}.txt").await?,
            Some("new => name.txt".into())
        );
        assert_eq!(
            snapshot.current_file_path("@", "new => name.txt").await?,
            Some("new => name.txt".into())
        );
        assert_eq!(snapshot.current_file_path("@", "missing.txt").await?, None);
        assert!(snapshot.current_file_path("@", "../outside").await.is_err());
        let files = snapshot
            .changed_files(&DiffScope::Change("@".into()))
            .await?;
        assert_eq!(
            files,
            vec![ChangedFile {
                path: "new => name.txt".into(),
                change_type: ChangeKind::Renamed,
                renamed_from: Some("old {name}.txt".into()),
                has_conflict: false,
            }]
        );

        Ok(())
    })
}

#[test]
fn search_parses_real_diff_paths_and_keeps_added_and_removed_line_numbers() -> Result<()> {
    smol::block_on(async {
        let fixture = Fixture::create()?;
        let paths = [
            "日本語.txt",
            "quote\".txt",
            "tab\tline\n.txt",
            "space name.txt",
        ];
        for path in paths {
            fs::write(fixture.root().join(path), "before\ncontext\n")?;
        }

        fixture.run(&["new"])?;
        for path in paths {
            fs::write(fixture.root().join(path), "after\nsecond\ncontext\n")?;
        }

        let snapshot = fixture.repository().await?.snapshot(true).await?;
        let (lines, _) = snapshot.search_data(&DiffScope::Change("@".into())).await?;
        for path in paths {
            let file_lines: Vec<_> = lines.iter().filter(|line| line.path == path).collect();
            assert_eq!(file_lines.len(), 3, "path: {path:?}");
            assert_eq!(file_lines[0].kind, LineKind::Removed);
            assert_eq!(file_lines[0].line_number, 1);
            assert_eq!(file_lines[1].kind, LineKind::Added);
            assert_eq!(file_lines[2].line_number, 2);
        }

        Ok(())
    })
}

#[test]
fn discovery_finds_sibling_repositories_and_excludes_foreign_metadata() -> Result<()> {
    smol::block_on(async {
        let container = tempfile::tempdir()?;
        let first = Fixture::from_directory(tempfile::tempdir_in(container.path())?)?;
        let second = Fixture::from_directory(tempfile::tempdir_in(container.path())?)?;
        let foreign = container.path().join(".jj");
        fs::create_dir(&foreign)?;
        let hidden = Fixture::from_directory(tempfile::tempdir_in(&foreign)?)?;
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(first.root(), container.path().join("alias"))?;
            std::os::unix::fs::symlink(container.path(), container.path().join("cycle"))?;
            std::os::unix::fs::symlink(&foreign, container.path().join("foreign-alias"))?;
        }

        let discovery = discover(container.path(), Path::new("kk")).await;
        assert!(discovery.errors.is_empty(), "{:?}", discovery.errors);
        assert_eq!(discovery.repositories.len(), 2);
        let roots: Vec<_> = discovery
            .repositories
            .iter()
            .map(Repository::root)
            .collect();
        assert!(roots.contains(&fs::canonicalize(first.root())?.as_path()));
        assert!(roots.contains(&fs::canonicalize(second.root())?.as_path()));
        assert!(!roots.contains(&fs::canonicalize(hidden.root())?.as_path()));

        Ok(())
    })
}

#[test]
fn revision_search_ranks_exact_matches_and_keeps_queries_literal_and_scoped() -> Result<()> {
    smol::block_on(async {
        let fixture = Fixture::create()?;
        fixture.run(&["describe", "-m", "needle"])?;
        fixture.run(&["new", "-m", "needle prefix"])?;
        fixture.run(&["new", "-m", "subject\nbody has needle"])?;
        fixture.run(&["new", "-m", "working copy needle"])?;
        let snapshot = fixture.repository().await?.snapshot(false).await?;
        let revisions = snapshot
            .search_revisions("needle", Some("..@"), true, 20)
            .await?;
        assert_eq!(revisions.len(), 4);
        assert_eq!(revisions[0].subject(), "needle");
        assert_eq!(revisions[1].subject(), "needle prefix");
        let without_working_copy = snapshot.search_revisions("needle", None, false, 20).await?;
        assert_eq!(without_working_copy.len(), 3);
        assert!(
            without_working_copy
                .iter()
                .all(|revision| !revision.is_working_copy)
        );
        assert!(
            snapshot
                .search_revisions("\") | all() | (\"", None, true, 20)
                .await?
                .is_empty()
        );
        let exact = snapshot
            .search_revisions(&revisions[0].full_change_id, Some("..@"), true, 20)
            .await?;
        assert_eq!(exact.len(), 1);
        assert_eq!(exact[0].full_commit_id, revisions[0].full_commit_id);

        Ok(())
    })
}

#[test]
fn change_id_prefixes_match_kk_across_history_and_search() -> Result<()> {
    smol::block_on(async {
        let fixture = Fixture::create()?;
        fixture.run(&["new", "-m", "prefix metadata"])?;
        let repository = fixture.repository().await?;
        let snapshot = repository.snapshot(true).await?;
        let page = snapshot.history(HistoryMode::Log, 50).await?;
        let revision = page.revisions.first().context("revision")?;
        let expected = fixture.run(&[
            "log",
            "--no-graph",
            "-r",
            &revision.full_commit_id,
            "-T",
            "change_id.shortest(8).prefix()",
        ])?;
        assert!(!expected.is_empty());
        assert_eq!(revision.change_id_prefix, expected);
        assert!(revision.change_id.starts_with(&expected));
        let found = snapshot
            .search_revisions(&revision.full_change_id, None, true, 20)
            .await?;
        let found = found
            .iter()
            .find(|item| item.full_commit_id == revision.full_commit_id)
            .context("search result")?;
        assert_eq!(found.change_id_prefix, expected);
        let mut old = serde_json::to_value(revision)?;
        old.as_object_mut()
            .context("revision object")?
            .remove("changeIdPrefix");
        let restored: Revision = serde_json::from_value(old)?;
        assert!(restored.change_id_prefix.is_empty());
        Ok(())
    })
}

#[test]
fn conflict_titles_preserve_long_and_unverified_change_ids() -> Result<()> {
    let mut input: ConflictInput = serde_json::from_value(serde_json::json!({
        "role":"Side", "index":0, "id":null, "executable":false, "bytes":[],
        "source":{"change_id":"kkkkkkkkklmnopqrstuvwxyz", "display_change_id":"kkkkkkkkkl", "change_id_prefix":"kkkkkkkkk", "commit_id":"abcdef", "description":"", "bookmarks":[], "role":null, "details_incomplete":false}
    }))?;
    assert_eq!(input.title(0), "kkkkkkkkkl");
    let source = input.source.as_mut().context("source")?;
    source.display_change_id = None;
    source.details_incomplete = true;
    assert_eq!(input.title(0), "kkkkkkkkklmnopqrstuvwxyz");
    Ok(())
}
