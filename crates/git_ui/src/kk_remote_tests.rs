use super::*;
use anyhow::{Context as _, Result, ensure};
use client::{Client, UserStore};
use clock::FakeSystemClock;
use gpui::TestAppContext;
use http_client::{BlockedHttpClient, FakeHttpClient};
use language::LanguageRegistry;
use node_runtime::NodeRuntime;
use project::{FakeFs, Project};
use remote::RemoteClient;
use remote_server::{HeadlessAppState, HeadlessProject};
use std::sync::Arc;
use workspace::MultiWorkspace;

pub(crate) struct RemoteFixture {
    pub directory: tempfile::TempDir,
    pub root: PathBuf,
    pub project: Entity<Project>,
    pub headless: Entity<HeadlessProject>,
}

impl RemoteFixture {
    pub async fn create(cx: &mut TestAppContext, server_cx: &mut TestAppContext) -> Result<Self> {
        cx.executor().allow_parking();
        let directory = cx
            .background_spawn(async {
                let directory = tempfile::tempdir()?;
                for arguments in [
                    vec!["init", "."],
                    vec!["config", "set", "--repo", "user.name", "Remote KK UI Test"],
                    vec![
                        "config",
                        "set",
                        "--repo",
                        "user.email",
                        "test@example.invalid",
                    ],
                ] {
                    run_kk(directory.path(), &arguments).await?;
                }

                std::fs::write(directory.path().join("file.txt"), b"base\n")?;
                run_kk(directory.path(), &["new", "-m", "source"]).await?;
                std::fs::write(directory.path().join("file.txt"), b"captured\n")?;
                anyhow::Ok(directory)
            })
            .await?;
        let root = std::fs::canonicalize(directory.path())?;
        cx.update(|cx| {
            release_channel::init("0.0.0".parse().expect("test version"), cx);
            let mut settings = SettingsStore::test(cx);
            settings
                .set_user_settings(r#"{"kk_workbench":{"auto_refresh":false}}"#, cx)
                .expect("test settings");
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        server_cx.update(|cx| {
            release_channel::init("0.0.0".parse().expect("test version"), cx);
            HeadlessProject::init(cx);
        });
        let (options, session, _) = RemoteClient::fake_server(cx, server_cx);
        let server_fs = fs::RealFs::new(None, server_cx.executor());
        let headless = server_cx.new(|cx| {
            HeadlessProject::new(
                HeadlessAppState {
                    session,
                    fs: server_fs,
                    http_client: Arc::new(BlockedHttpClient),
                    node_runtime: NodeRuntime::unavailable(),
                    languages: Arc::new(LanguageRegistry::new(cx.background_executor().clone())),
                    extension_host_proxy: Arc::new(extension::ExtensionHostProxy::new()),
                    startup_time: std::time::Instant::now(),
                },
                false,
                cx,
            )
        });
        let remote = RemoteClient::connect_mock(options, cx).await;
        let client = cx.update(|cx| {
            Client::new(
                Arc::new(FakeSystemClock::new()),
                FakeHttpClient::with_404_response(),
                cx,
            )
        });
        let user_store = cx.new(|cx| UserStore::new(client.clone(), cx));
        let languages = Arc::new(LanguageRegistry::test(cx.executor()));
        let client_fs = FakeFs::new(cx.executor());
        let project = cx.update(|cx| {
            Project::init(&client, cx);
            Project::remote(
                remote,
                client,
                NodeRuntime::unavailable(),
                user_store,
                languages,
                client_fs,
                false,
                cx,
            )
        });
        project
            .update(cx, |project, cx| {
                project.find_or_create_worktree(&root, true, cx)
            })
            .await?;
        Ok(Self {
            directory,
            root,
            project,
            headless,
        })
    }
}

pub(crate) async fn run_kk(root: &Path, arguments: &[&str]) -> Result<()> {
    let output = util::command::new_command("kk")
        .current_dir(root)
        .args(arguments)
        .output()
        .await?;
    ensure!(
        output.status.success(),
        "kk {arguments:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

#[gpui::test(iterations = 3)]
async fn kk_remote_panel_discovers_opens_classifies_and_preserves_later_saves(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    run_remote_panel(cx, server_cx)
        .await
        .expect("remote panel workflow");
}

async fn run_remote_panel(cx: &mut TestAppContext, server_cx: &mut TestAppContext) -> Result<()> {
    let fixture = RemoteFixture::create(cx, server_cx).await?;
    let project = fixture.project.clone();
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = multi_workspace.read_with(cx, |view, _| view.workspace().clone());
    let panel = cx
        .update(|window, cx| KkPanel::load(workspace.downgrade(), window.to_async(cx)))
        .await?;
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.add_panel(panel.clone(), window, cx)
    });
    panel
        .update(cx, |panel, cx| panel.refresh_for_test(cx))
        .await;
    let snapshot = panel.read_with(cx, |panel, _| {
        assert!(panel.error.is_none(), "{:?}", panel.error);
        assert_eq!(panel.repositories.len(), 1);
        panel.repositories[0].snapshot.clone()
    });
    assert!(snapshot.repository().is_remote());
    assert_eq!(snapshot.repository().root(), fixture.root);
    let error_status = workspace.read_with(cx, |workspace, cx| {
        workspace
            .status_bar()
            .read(cx)
            .item_of_type::<KkErrorStatus>()
            .expect("KK error status")
    });
    panel.update(cx, |panel, cx| {
        panel.operation_error = Some((
            fixture.root.clone(),
            "Operation failed\nFull failure details".into(),
        ));
        cx.notify();
    });
    cx.run_until_parked();
    panel
        .update(cx, |panel, cx| panel.refresh_for_test(cx))
        .await;
    assert_eq!(
        error_status
            .read_with(cx, |status, cx| status.message(cx))
            .as_deref(),
        Some("Operation failed\nFull failure details")
    );
    assert!(panel.read_with(cx, |panel, cx| {
        panel
            .output
            .read(cx)
            .diagnostics()
            .read_since(0)
            .1
            .contains("Full failure details")
    }));
    panel.update(cx, |panel, cx| {
        panel.operation_error = None;
        cx.notify();
    });
    assert!(
        error_status
            .read_with(cx, |status, cx| status.message(cx))
            .is_none()
    );
    let parent = snapshot.exact_revision("@-").await?;
    let comparison = Comparison::load(&snapshot, Comparison::new(&parent).intent).await?;
    let file = comparison.files.first().context("changed file")?.clone();
    panel.update(cx, |panel, _| {
        panel.repositories[0].comparison = Some(comparison.clone())
    });
    let path = project
        .read_with(cx, |project, cx| {
            project.find_project_path(&fixture.root.join("file.txt"), cx)
        })
        .context("remote file path")?;
    let buffer = project
        .update(cx, |project, cx| project.open_buffer(path, cx))
        .await?;
    assert!(!buffer.read_with(cx, |buffer, _| {
        buffer.file().expect("remote file").is_local()
    }));
    let diff = cx
        .update(|window, cx| {
            KkDiff::open(
                snapshot.clone(),
                comparison.diff_target(),
                file.clone(),
                true,
                workspace.downgrade(),
                DiffOpenMode::Open,
                window,
                cx,
            )
        })
        .await?;
    let editor = cx.update(|_, cx| {
        use workspace::item::ItemHandle as _;
        diff.act_as_type(std::any::TypeId::of::<editor::Editor>(), cx)
            .context("diff editor")?
            .downcast::<editor::Editor>()
            .map_err(|_| anyhow::anyhow!("editor type"))
    })?;
    assert_eq!(
        editor.read_with(cx, |editor, cx| editor.active_buffer(cx)),
        Some(buffer.clone())
    );
    buffer.update(cx, |buffer, cx| {
        buffer.edit([(0..0, "draft\n")], None, cx);
    });
    panel.read_with(cx, |panel, cx| {
        assert!(panel.file_is_dirty(&fixture.root, &file, cx));
        assert!(panel.check_conflict_write(&fixture.root, cx).is_err());
    });
    buffer.update(cx, |buffer, cx| {
        buffer.edit([(0..6, "")], None, cx);
    });
    assert!(!buffer.read_with(cx, |buffer, _| buffer.is_dirty()));
    let classified = panel.update(cx, |panel, cx| {
        panel.classify_files_result(
            snapshot.clone(),
            comparison.diff_target(),
            comparison.intent.clone(),
            vec![FileSelection {
                file: file.clone(),
                ranges: None,
            }],
            ClassificationTarget::Category(Category::Reviewed),
            cx,
        )
    });
    buffer.update(cx, |buffer, cx| {
        buffer.edit([(0..buffer.len(), "later saved\n")], None, cx);
    });
    project
        .update(cx, |project, cx| project.save_buffer(buffer.clone(), cx))
        .await?;
    let published = classified
        .await
        .map_err(|error| anyhow::anyhow!(error.message))?;
    let category = published
        .revisions(Some("description(substring:\"KKW: REVIEWED\")"), 10)
        .await?
        .into_iter()
        .find(|revision| revision.full_description.trim_end() == "KKW: REVIEWED")
        .context("reviewed category")?;
    assert_eq!(
        published
            .file_content(&category.revset(), "file.txt")
            .await?,
        b"captured\n"
    );
    assert_eq!(
        std::fs::read(fixture.root.join("file.txt"))?,
        b"later saved\n"
    );
    assert_eq!(
        buffer.read_with(cx, |buffer, _| buffer.text()),
        "later saved\n"
    );
    panel
        .update(cx, |panel, cx| panel.refresh_for_test(cx))
        .await;
    panel.read_with(cx, |panel, _| {
        assert!(panel.error.is_none(), "{:?}", panel.error);
        assert!(
            panel.operation_error.is_none(),
            "{:?}",
            panel.operation_error
        );
        assert_eq!(panel.pending_writes, 0);
        assert_eq!(
            panel.repositories[0]
                .comparison
                .as_ref()
                .expect("comparison retained")
                .categories
                .len(),
            1
        );
    });
    cx.run_until_parked();
    let output = panel.read_with(cx, |panel, cx| panel.diagnostics(cx));
    assert!(output.read_since(0).1.contains("[COMMAND]"));
    let lifetime = snapshot
        .repository()
        .lifetime()
        .context("root lifetime")?
        .clone();
    let worktrees = project.read_with(cx, |project, cx| {
        project
            .visible_worktrees(cx)
            .map(|tree| tree.read(cx).id())
            .collect::<Vec<_>>()
    });
    project.update(cx, |project, cx| {
        for worktree in worktrees {
            project.remove_worktree(worktree, cx);
        }
    });
    cx.run_until_parked();
    assert!(lifetime.check().is_err());
    assert!(snapshot.repository().snapshot(true).await.is_err());
    assert_eq!(snapshot.file_content("@", "file.txt").await?, b"captured\n");
    assert!(fixture.directory.path().exists());
    fixture.headless.read_with(server_cx, |_, _| {});
    Ok(())
}
