use super::*;
use anyhow::ensure;
use gpui::TestAppContext;
use kk_workbench::{
    Category, ClassificationRequest, ClassificationTarget, Diagnostics, DiffScope, FileSelection,
    RepositoryLifetime, SavedSnapshot, remote::Request,
};
use util::rel_path::rel_path;

#[gpui::test(iterations = 3)]
async fn kk_remote_rpc_classifies_saved_bytes_and_preserves_later_buffer_saves(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    run_remote_saved_classification(cx, server_cx)
        .await
        .expect("remote saved classification");
}

async fn run_remote_saved_classification(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) -> Result<()> {
    cx.executor().allow_parking();
    let directory = cx
        .background_spawn(async {
            let directory = tempfile::tempdir()?;
            for arguments in [
                vec!["init", "."],
                vec!["config", "set", "--repo", "user.name", "Remote KK Test"],
                vec![
                    "config",
                    "set",
                    "--repo",
                    "user.email",
                    "test@example.invalid",
                ],
            ] {
                let output = smol::process::Command::new("kk")
                    .current_dir(directory.path())
                    .args(arguments)
                    .output()
                    .await?;
                ensure!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }

            std::fs::write(directory.path().join("file.txt"), b"base\n")?;
            let output = smol::process::Command::new("kk")
                .current_dir(directory.path())
                .args(["new", "-m", "source"])
                .output()
                .await?;
            ensure!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            std::fs::write(directory.path().join("file.txt"), b"captured\n")?;
            anyhow::Ok(directory)
        })
        .await?;
    let root = std::fs::canonicalize(directory.path())?;
    let server_fs = fs::RealFs::new(None, server_cx.executor());
    let (project, headless) =
        crate::remote_editing_tests::init_test_with_fs(server_fs, cx, server_cx).await;
    let service = headless.read_with(server_cx, |headless, _| headless.kk_workbench.clone());
    let client = project.read_with(cx, |project, cx| {
        project
            .remote_client()
            .expect("remote client")
            .read(cx)
            .proto_client()
    });
    assert!(
        client
            .request(proto::KkWorkbenchRequest { payload: vec![255] })
            .await
            .is_err()
    );
    let remote = project
        .read_with(cx, |project, cx| project.kk_workbench_client(cx))
        .context("KK transport")?;
    let repository = remote
        .open(&root, Path::new("kk"), Diagnostics::default())
        .await?;
    let lifetime = RepositoryLifetime::default();
    let original = repository
        .snapshot(true)
        .await?
        .with_lifetime(Some(lifetime.clone()));
    let files = original
        .changed_files(&DiffScope::Change("@".into()))
        .await?;
    let (worktree, _) = project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(&root, true, cx)
        })
        .await?;
    let worktree_id = worktree.read_with(cx, |worktree, _| worktree.id());
    let buffer = project
        .update(cx, |project, cx| {
            project.open_buffer((worktree_id, rel_path("file.txt")), cx)
        })
        .await?;
    let captured = SavedSnapshot::capture(original.repository(), ["file.txt".into()])?;
    buffer.update(cx, |buffer, cx| {
        assert_eq!(buffer.text(), "captured\n");
        buffer.edit([(0..buffer.len(), "later saved\n")], None, cx);
    });
    project
        .update(cx, |project, cx| project.save_buffer(buffer.clone(), cx))
        .await?;
    assert_eq!(std::fs::read(root.join("file.txt"))?, b"later saved\n");
    let refreshed_client = project
        .read_with(cx, |project, cx| project.kk_workbench_client(cx))
        .context("refreshed KK transport")?;
    let refreshed_repository = refreshed_client
        .open(&root, Path::new("kk"), Diagnostics::default())
        .await?;
    let refreshed = refreshed_repository.snapshot_before_write().await?;
    let prepared = refreshed
        .prepare_classification_with_saved(
            &original,
            &ClassificationRequest {
                comparison: "@-".into(),
                range_start: None,
                source: None,
                target: ClassificationTarget::Category(Category::Reviewed),
                files: files
                    .into_iter()
                    .map(|file| FileSelection { file, ranges: None })
                    .collect(),
            },
            vec![captured.clone()],
        )
        .await?
        .context("prepared classification")?;
    assert_eq!(
        captured.files()?["file.txt"].bytes.as_deref(),
        Some(b"captured\n".as_slice())
    );
    let published = prepared.publish().await?;
    assert_eq!(
        published
            .publication
            .snapshot
            .file_content(&published.destination.revset(), "file.txt")
            .await?,
        b"captured\n"
    );
    assert_eq!(std::fs::read(root.join("file.txt"))?, b"later saved\n");
    let finished = captured.finish().await?.context("later saved snapshot")?;
    assert_eq!(
        finished.file_content("@", "file.txt").await?,
        b"later saved\n"
    );
    assert_eq!(original.file_content("@", "file.txt").await?, b"captured\n");
    lifetime.invalidate();
    assert!(original.repository().snapshot(true).await.is_err());
    drop(finished);
    drop(published);
    drop(captured);
    drop(original);
    drop(repository);
    drop(refreshed);
    drop(refreshed_repository);
    drop(refreshed_client);
    drop(remote);
    drop(buffer);
    drop(worktree);
    drop(project);
    drop(headless);
    cx.update(|_| {});
    server_cx.update(|_| {});
    cx.run_until_parked();
    server_cx.run_until_parked();
    assert!(
        service
            .execute(Request::Diagnostics {
                cursor: None,
                wait: false
            })
            .await
            .is_err()
    );
    Ok(())
}
