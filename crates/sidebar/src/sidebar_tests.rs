use super::*;
use acp_thread::{AcpThread, PermissionOptions, StubAgentConnection};
use agent::ThreadStore;
use agent_client_protocol::{v1::schema as acp_v1, v2::schema as acp_v2};
use agent_settings::AgentSettings;
use agent_ui::{
    ThreadId,
    terminal_thread_metadata_store::{
        TerminalThreadMetadata, TerminalThreadMetadataStore, TestTerminalMetadataDbName,
    },
    test_support::{
        active_session_id, active_thread_id, open_thread_with_connection,
        open_thread_with_custom_connection, send_message,
    },
    thread_metadata_store::{ThreadMetadata, WorktreePaths},
};
use chrono::DateTime;
use fs::{FakeFs, Fs};
use gpui::{App, Bounds, Hsla, Point, Rgba, TestAppContext, UpdateGlobal, VisualTestContext};
use pretty_assertions::assert_eq;
use project::AgentId;
use settings::{Settings, SettingsStore};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use util::path_list::PathList;

fn use_unique_metadata_databases(cx: &mut TestAppContext) {
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_TEST_DATABASE: AtomicUsize = AtomicUsize::new(0);
    let test_database_id = NEXT_TEST_DATABASE.fetch_add(1, Ordering::SeqCst);
    cx.update(|cx| {
        cx.set_global(agent_ui::thread_metadata_store::TestMetadataDbName(
            format!("SIDEBAR_THREAD_METADATA_{test_database_id}"),
        ));
        cx.set_global(TestTerminalMetadataDbName(format!(
            "SIDEBAR_TERMINAL_THREAD_METADATA_{test_database_id}"
        )));
    });
}

fn set_max_idle_retained_threads(max_idle_retained_threads: usize, cx: &mut App) {
    AgentSettings::override_global(
        AgentSettings {
            max_idle_retained_threads,
            ..AgentSettings::get_global(cx).clone()
        },
        cx,
    );
}

fn init_test(cx: &mut TestAppContext) {
    use_unique_metadata_databases(cx);
    cx.update(|cx| {
        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);
        // Use an isolated DB so parallel tests can't see each other's
        // persisted records (e.g. created-worktree records).
        cx.set_global(db::AppDatabase::test_new());
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        editor::init(cx);
        ThreadStore::init_global(cx);
        ThreadMetadataStore::init_global(cx);
        TerminalThreadMetadataStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
        prompt_store::init(cx);
    });
}

#[track_caller]
fn assert_active_thread(sidebar: &Sidebar, session_id: &acp::SessionId, msg: &str) {
    let active = sidebar.active_entry.as_ref();
    let matches = active.is_some_and(|entry| {
        matches!(entry, ActiveEntry::Thread { session_id: Some(active_session_id), .. } if active_session_id == session_id)
            || sidebar.contents.entries.iter().any(|list_entry| {
                matches!(list_entry, ListEntry::Thread(t)
                    if t.metadata.session_id.as_ref() == Some(session_id)
                        && entry.matches_entry(list_entry))
            })
    });
    assert!(
        matches,
        "{msg}: expected active_entry for session {session_id:?}, got {:?}",
        active,
    );
}

#[track_caller]
fn is_active_session(sidebar: &Sidebar, session_id: &acp::SessionId) -> bool {
    let thread_id = sidebar
        .contents
        .entries
        .iter()
        .find_map(|entry| match entry {
            ListEntry::Thread(t) if t.metadata.session_id.as_ref() == Some(session_id) => {
                Some(t.metadata.thread_id)
            }
            _ => None,
        });
    match thread_id {
        Some(tid) => {
            matches!(&sidebar.active_entry, Some(ActiveEntry::Thread { thread_id, .. }) if *thread_id == tid)
        }
        // Thread not in sidebar entries — can't confirm it's active.
        None => false,
    }
}

#[track_caller]
fn assert_active_draft(sidebar: &Sidebar, workspace: &Entity<Workspace>, msg: &str) {
    assert!(
        matches!(&sidebar.active_entry, Some(ActiveEntry::Thread { workspace: ws, .. }) if ws == workspace),
        "{msg}: expected active_entry to be Draft for workspace {:?}, got {:?}",
        workspace.entity_id(),
        sidebar.active_entry,
    );
}

fn has_thread_entry(sidebar: &Sidebar, session_id: &acp::SessionId) -> bool {
    sidebar
        .contents
        .entries
        .iter()
        .any(|entry| matches!(entry, ListEntry::Thread(t) if t.metadata.session_id.as_ref() == Some(session_id)))
}

#[track_caller]
fn assert_project_header_has_threads(
    sidebar: &Entity<Sidebar>,
    project_name: &str,
    expected_has_threads: bool,
    cx: &mut gpui::VisualTestContext,
) {
    sidebar.read_with(cx, |sidebar, _cx| {
        let has_threads = sidebar.contents.entries.iter().find_map(|entry| {
            if let ListEntry::ProjectHeader {
                label, has_threads, ..
            } = entry
                && label.as_ref() == project_name
            {
                Some(*has_threads)
            } else {
                None
            }
        });

        assert_eq!(
            has_threads,
            Some(expected_has_threads),
            "expected project header `{project_name}` to have has_threads={expected_has_threads}, got {has_threads:?}"
        );
    });
}
async fn init_test_project(
    worktree_path: &str,
    cx: &mut TestAppContext,
) -> Entity<project::Project> {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(worktree_path, serde_json::json!({ "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
    project::Project::test(fs, [worktree_path.as_ref()], cx).await
}

#[gpui::test]
async fn test_workspace_menu_uses_opened_directory_name(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        "/zed/.bare",
        serde_json::json!({
            "worktrees": {
                "glossy-walrus": {
                    "commondir": "../..",
                    "HEAD": "ref: refs/heads/glossy-walrus",
                },
            },
        }),
    )
    .await;
    fs.insert_tree(
        "/worktrees/zed/glossy-walrus/zed",
        serde_json::json!({
            ".git": "gitdir: /zed/.bare/worktrees/glossy-walrus",
            "src": {},
        }),
    )
    .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let project =
        project::Project::test(fs, [Path::new("/worktrees/zed/glossy-walrus/zed")], cx).await;
    project
        .update(cx, |project, cx| project.wait_for_initial_scan(cx))
        .await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = multi_workspace.read_with(cx, |multi_workspace, _cx| {
        multi_workspace.workspace().clone()
    });
    let labels = cx.update(|_window, cx| workspace_menu_worktree_labels(&workspace, cx));

    assert_eq!(labels.len(), 1);
    assert_eq!(labels[0].primary_name.as_ref(), "zed");
    assert_eq!(labels[0].secondary_name, None);
}

fn setup_sidebar(
    multi_workspace: &Entity<MultiWorkspace>,
    cx: &mut gpui::VisualTestContext,
) -> Entity<Sidebar> {
    let sidebar = setup_sidebar_closed(multi_workspace, cx);
    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.toggle_sidebar(window, cx);
    });
    cx.run_until_parked();
    sidebar
}

fn setup_sidebar_closed(
    multi_workspace: &Entity<MultiWorkspace>,
    cx: &mut gpui::VisualTestContext,
) -> Entity<Sidebar> {
    let multi_workspace = multi_workspace.clone();
    let sidebar =
        cx.update(|window, cx| cx.new(|cx| Sidebar::new(multi_workspace.clone(), window, cx)));
    multi_workspace.update(cx, |mw, cx| {
        mw.register_sidebar(sidebar.clone(), cx);
    });
    cx.run_until_parked();
    sidebar
}

fn set_threads_sidebar_default_width(width: f32, cx: &mut App) {
    SettingsStore::update_global(cx, |store, cx| {
        store
            .set_user_settings(
                &format!(r#"{{"agent": {{"threads_sidebar": {{"default_width": {width}}}}}}}"#),
                cx,
            )
            .unwrap();
    });
}

async fn save_n_test_threads(
    count: u32,
    project: &Entity<project::Project>,
    cx: &mut gpui::VisualTestContext,
) {
    for i in 0..count {
        save_thread_metadata(
            acp::SessionId::new(Arc::from(format!("thread-{}", i))),
            Some(format!("Thread {}", i + 1).into()),
            chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, i).unwrap(),
            None,
            None,
            project,
            cx,
        )
    }
    cx.run_until_parked();
}

async fn save_test_thread_metadata(
    session_id: &acp::SessionId,
    project: &Entity<project::Project>,
    cx: &mut TestAppContext,
) {
    save_thread_metadata(
        session_id.clone(),
        Some("Test".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        None,
        None,
        project,
        cx,
    )
}

async fn save_named_thread_metadata(
    session_id: &str,
    title: &str,
    project: &Entity<project::Project>,
    cx: &mut gpui::VisualTestContext,
) {
    save_thread_metadata(
        acp::SessionId::new(Arc::from(session_id)),
        Some(SharedString::from(title.to_string())),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        None,
        None,
        project,
        cx,
    );
    cx.run_until_parked();
}

/// Seeds a pre-built [`ThreadMetadata`] into the global store so tests can
/// exercise flows that resolve a thread by id.
fn seed_thread_metadata(metadata: ThreadMetadata, cx: &mut TestAppContext) {
    cx.update(|cx| {
        ThreadMetadataStore::global(cx).update(cx, |store, cx| store.save(metadata, cx));
    });
    cx.run_until_parked();
}

/// Spins up a fresh remote project backed by a headless server sharing
/// `server_fs`, opens the given worktree path on it, and returns the
/// project together with the headless entity (which the caller must keep
/// alive for the duration of the test) and the `RemoteConnectionOptions`
/// used for the fake server. Passing those options back into
/// `reuse_opts` on a subsequent call makes the new project share the
/// same `RemoteConnectionIdentity`, matching how Zed treats multiple
/// projects on the same SSH host.
async fn start_remote_project(
    server_fs: &Arc<FakeFs>,
    worktree_path: &Path,
    app_state: &Arc<workspace::AppState>,
    reuse_opts: Option<&remote::RemoteConnectionOptions>,
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) -> (
    Entity<project::Project>,
    Entity<remote_server::HeadlessProject>,
    remote::RemoteConnectionOptions,
) {
    // Bare `_` on the guard so it's dropped immediately; holding onto it
    // would deadlock `connect_mock` below since the client waits on the
    // guard before completing the mock handshake.
    let (opts, server_session) = match reuse_opts {
        Some(existing) => {
            let (session, _) = remote::RemoteClient::fake_server_with_opts(existing, cx, server_cx);
            (existing.clone(), session)
        }
        None => {
            let (opts, session, _) = remote::RemoteClient::fake_server(cx, server_cx);
            (opts, session)
        }
    };

    server_cx.update(remote_server::HeadlessProject::init);
    let server_executor = server_cx.executor();
    let fs = server_fs.clone();
    let headless = server_cx.new(|cx| {
        remote_server::HeadlessProject::new(
            remote_server::HeadlessAppState {
                session: server_session,
                fs,
                http_client: Arc::new(http_client::BlockedHttpClient),
                node_runtime: node_runtime::NodeRuntime::unavailable(),
                languages: Arc::new(language::LanguageRegistry::new(server_executor.clone())),
                extension_host_proxy: Arc::new(extension::ExtensionHostProxy::new()),
                startup_time: std::time::Instant::now(),
            },
            false,
            cx,
        )
    });

    let remote_client = remote::RemoteClient::connect_mock(opts.clone(), cx).await;
    let project = cx.update(|cx| {
        let project_client = client::Client::new(
            Arc::new(clock::FakeSystemClock::new()),
            http_client::FakeHttpClient::with_404_response(),
            cx,
        );
        let user_store = cx.new(|cx| client::UserStore::new(project_client.clone(), cx));
        project::Project::remote(
            remote_client,
            project_client,
            node_runtime::NodeRuntime::unavailable(),
            user_store,
            app_state.languages.clone(),
            app_state.fs.clone(),
            false,
            cx,
        )
    });

    project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(worktree_path, true, cx)
        })
        .await
        .expect("should open remote worktree");
    cx.run_until_parked();

    (project, headless, opts)
}

fn save_thread_metadata(
    session_id: acp::SessionId,
    title: Option<SharedString>,
    updated_at: DateTime<Utc>,
    created_at: Option<DateTime<Utc>>,
    interacted_at: Option<DateTime<Utc>>,
    project: &Entity<project::Project>,
    cx: &mut TestAppContext,
) {
    cx.update(|cx| {
        let worktree_paths = project.read(cx).worktree_paths(cx);
        let remote_connection = project.read(cx).remote_connection_options(cx).cloned();
        let thread_id = ThreadMetadataStore::global(cx)
            .read(cx)
            .entries()
            .find(|e| e.session_id.as_ref() == Some(&session_id))
            .map(|e| e.thread_id)
            .unwrap_or_else(ThreadId::new);
        let metadata = ThreadMetadata {
            thread_id,
            session_id: Some(session_id),
            agent_id: agent::ZED_AGENT_ID.clone(),
            title,
            title_override: None,
            updated_at,
            created_at,
            interacted_at,
            worktree_paths,
            archived: false,
            remote_connection,
        };
        ThreadMetadataStore::global(cx).update(cx, |store, cx| store.save(metadata, cx));
    });
    cx.run_until_parked();
}

fn save_thread_metadata_with_main_paths(
    session_id: &str,
    title: &str,
    folder_paths: PathList,
    main_worktree_paths: PathList,
    updated_at: DateTime<Utc>,
    cx: &mut TestAppContext,
) {
    let session_id = acp::SessionId::new(Arc::from(session_id));
    let title = SharedString::from(title.to_string());
    let thread_id = cx.update(|cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entries()
            .find(|e| e.session_id.as_ref() == Some(&session_id))
            .map(|e| e.thread_id)
            .unwrap_or_else(ThreadId::new)
    });
    let metadata = ThreadMetadata {
        thread_id,
        session_id: Some(session_id),
        agent_id: agent::ZED_AGENT_ID.clone(),
        title: Some(title),
        title_override: None,
        updated_at,
        created_at: None,
        interacted_at: None,
        worktree_paths: WorktreePaths::from_path_lists(main_worktree_paths, folder_paths).unwrap(),
        archived: false,
        remote_connection: None,
    };
    cx.update(|cx| {
        ThreadMetadataStore::global(cx).update(cx, |store, cx| store.save(metadata, cx));
    });
    cx.run_until_parked();
}
fn focus_sidebar(sidebar: &Entity<Sidebar>, cx: &mut gpui::VisualTestContext) {
    sidebar.update_in(cx, |_, window, cx| {
        cx.focus_self(window);
    });
    cx.run_until_parked();
}

fn enter_renamed_title(
    sidebar: &Entity<Sidebar>,
    target: RenameTarget,
    renamed_title: &str,
    cx: &mut gpui::VisualTestContext,
) {
    sidebar.read_with(cx, |sidebar, _cx| {
        assert_eq!(sidebar.rename_target, Some(target));
    });
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.rename_editor.update(cx, |editor, cx| {
            editor.set_text(renamed_title, window, cx);
        });
    });
    cx.run_until_parked();
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.finish_entry_rename(window, cx);
    });
    cx.run_until_parked();
}

fn request_test_tool_authorization(
    thread: &Entity<AcpThread>,
    tool_call_id: &str,
    option_id: &str,
    cx: &mut gpui::VisualTestContext,
) {
    let tool_call_id = acp::ToolCallId::new(tool_call_id);
    let label = format!("Tool {tool_call_id}");
    let option_id = acp_v2::PermissionOptionId::new(option_id);
    let _authorization_task = cx.update(|_, cx| {
        thread.update(cx, |thread, cx| {
            thread
                .request_tool_call_authorization(
                    acp_v1::ToolCall::new(acp_v1::ToolCallId::new(tool_call_id.0), label)
                        .kind(acp_v1::ToolKind::Edit)
                        .into(),
                    PermissionOptions::Flat(vec![acp_v2::PermissionOption::new(
                        option_id,
                        "Allow",
                        acp_v2::PermissionOptionKind::AllowOnce,
                    )]),
                    acp_thread::AuthorizationKind::PermissionGrant,
                    cx,
                )
                .unwrap()
        })
    });
    cx.run_until_parked();
}

fn format_linked_worktree_chips(worktrees: &[ThreadItemWorktreeInfo]) -> String {
    let mut seen = Vec::new();
    let mut chips = Vec::new();
    for wt in worktrees {
        if wt.kind == ui::WorktreeKind::Main {
            continue;
        }
        let Some(name) = wt.worktree_name.as_ref() else {
            continue;
        };
        if !seen.contains(name) {
            seen.push(name.clone());
            chips.push(format!("{{{}}}", name));
        }
    }
    if chips.is_empty() {
        String::new()
    } else {
        format!(" {}", chips.join(", "))
    }
}

fn visible_entries_as_strings(
    sidebar: &Entity<Sidebar>,
    cx: &mut gpui::VisualTestContext,
) -> Vec<String> {
    sidebar.read_with(cx, |sidebar, cx| {
        sidebar
            .contents
            .entries
            .iter()
            .enumerate()
            .map(|(ix, entry)| {
                let selected = if sidebar.selection == Some(ix) {
                    "  <== selected"
                } else {
                    ""
                };
                match entry {
                    ListEntry::ProjectHeader { label, key, .. } => {
                        let icon = if sidebar.is_group_collapsed(key, cx) {
                            ">"
                        } else {
                            "v"
                        };
                        format!("{} [{}]{}", icon, label, selected)
                    }
                    ListEntry::Thread(thread) => {
                        let title = thread.metadata.display_title();
                        let worktree = format_linked_worktree_chips(&thread.worktrees);

                        {
                            let live = if thread.is_live { " *" } else { "" };
                            let status_str = match thread.status {
                                AgentThreadStatus::Running => " (running)",
                                AgentThreadStatus::Error => " (error)",
                                AgentThreadStatus::WaitingForConfirmation => " (waiting)",
                                _ => "",
                            };
                            let notified = if sidebar
                                .contents
                                .is_thread_notified(&thread.metadata.thread_id)
                            {
                                " (!)"
                            } else {
                                ""
                            };
                            format!("  {title}{worktree}{live}{status_str}{notified}{selected}")
                        }
                    }
                    ListEntry::Terminal(terminal) => {
                        let title = terminal.metadata.display_title();
                        let worktree = format_linked_worktree_chips(&terminal.worktrees);
                        format!("  {title}{worktree}{selected}")
                    }
                }
            })
            .collect()
    })
}

#[gpui::test]
async fn test_sidebar_background_with_transparent_panel(cx: &mut TestAppContext) {
    let (_, project) = init_multi_project_test(&["/my-project"], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    for panel_alpha in [0.0, 0.4] {
        set_sidebar_test_panel_alpha(panel_alpha, cx);
        let background = cx.update(|_, cx| {
            let colors = cx.theme().colors();
            colors.background.blend(colors.panel_background)
        });
        cx.draw(
            gpui::point(px(0.), px(0.)),
            gpui::size(px(400.), px(240.)),
            |_, _| sidebar.clone().into_any_element(),
        );

        cx.update(|window, _| {
            let quads = window.painted_quads();
            let sidebar_background = quads.first().expect("Sidebar should paint its background");
            assert_eq!(sidebar_background.background.as_solid(), Some(background));
        });
    }
}

#[gpui::test]
async fn test_sidebar_action_hover_contrasts_with_row(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    cx.update(|cx| AgentRegistryStore::init_test_global(cx, Vec::new()));
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, _panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    save_n_test_threads(1, &project, cx).await;

    for (query, panel_alpha) in [("my-project", 1.0), ("", 1.0), ("", 0.0), ("", 0.2)] {
        set_sidebar_test_panel_alpha(panel_alpha, cx);
        type_in_search(&sidebar, query, cx);
        let row_bounds = sidebar.read_with(cx, |sidebar, _| {
            sidebar
                .list_state
                .bounds_for_item(0)
                .expect("rendered project header")
        });
        if query.is_empty() {
            cx.simulate_mouse_move(row_bounds.center(), None, Modifiers::default());
            let hover =
                cx.update(|_, cx| u32::from(Rgba::from(cx.theme().colors().ghost_element_hover)));
            assert_eq!(
                sidebar_painted_background_at(row_bounds.center(), cx),
                hover
            );
        }
        for selector in ["ICON-Plus", "ICON-Ellipsis"] {
            assert_sidebar_action_hover(selector, row_bounds, cx);
        }
        if query.is_empty() {
            let thread_bounds = sidebar.read_with(cx, |sidebar, _| {
                sidebar
                    .list_state
                    .bounds_for_item(1)
                    .expect("rendered thread")
            });
            for selector in ["ICON-Pencil", "ICON-Archive"] {
                assert_sidebar_action_hover(selector, thread_bounds, cx);
            }
        }
    }

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.show_archive(window, cx);
        let SidebarView::Archive(archive) = &sidebar.view else {
            panic!("Thread History should be open");
        };
        window.focus(&archive.focus_handle(cx), cx);
    });
    cx.run_until_parked();

    for (archived, selector) in [(false, "ICON-Archive"), (true, "ICON-Trash")] {
        if archived {
            cx.update(|_, cx| {
                ThreadMetadataStore::global(cx).update(cx, |store, cx| {
                    let thread_id = store.entries().next().expect("seeded thread").thread_id;
                    store.archive(thread_id, None, cx);
                });
            });
            cx.run_until_parked();
        }
        cx.dispatch_action(SelectFirst);
        let row_bounds = cx.update(|window, cx| {
            window
                .painted_quads()
                .into_iter()
                .find(|quad| quad.border_color == cx.theme().colors().panel_focused_border)
                .expect("selected Thread History row")
                .bounds
                .map(|value| px(value.as_f32() / window.scale_factor()))
        });
        for panel_alpha in [1.0, 0.0, 0.2] {
            set_sidebar_test_panel_alpha(panel_alpha, cx);
            assert_sidebar_action_hover(selector, row_bounds, cx);
        }
    }
}

#[gpui::test]
async fn test_thread_metadata_update_preserves_sticky_header_measurements(cx: &mut TestAppContext) {
    let (fs, project_a) = init_multi_project_test(&["/project-a", "/project-b"], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    add_test_project("/project-b", &fs, &multi_workspace, cx).await;

    save_thread_metadata(
        acp::SessionId::new(Arc::from("project-a-thread")),
        Some("Project A Thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        None,
        None,
        &project_a,
        cx,
    );
    save_thread_metadata_with_main_paths(
        "project-b-thread",
        "Project B Thread",
        PathList::new(&[PathBuf::from("/project-b")]),
        PathList::new(&[PathBuf::from("/project-b")]),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 2, 0, 0, 0).unwrap(),
        cx,
    );

    cx.draw(
        gpui::point(px(0.), px(0.)),
        gpui::size(px(400.), px(240.)),
        |_, _| sidebar.clone().into_any_element(),
    );
    cx.run_until_parked();

    let next_header_ix = sidebar.read_with(cx, |sidebar, _| {
        assert!(
            sidebar.contents.project_header_indices.len() == 2,
            "test setup should render exctly two project headers"
        );
        sidebar.contents.project_header_indices[1]
    });

    sidebar.update_in(cx, |sidebar, _window, cx| {
        sidebar.list_state.scroll_to(gpui::ListOffset {
            item_ix: next_header_ix - 1,
            offset_in_item: px(24.),
        });
        cx.notify();
    });
    cx.draw(
        gpui::point(px(0.), px(0.)),
        gpui::size(px(400.), px(240.)),
        |_, _| sidebar.clone().into_any_element(),
    );
    cx.run_until_parked();

    let bounds_before = sidebar.read_with(cx, |sidebar, _| {
        sidebar
            .list_state
            .bounds_for_item(next_header_ix)
            .expect("next project header should be measured before metadata update")
    });

    save_thread_metadata(
        acp::SessionId::new(Arc::from("project-a-thread")),
        Some("Renamed Project A Thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 1, 0).unwrap(),
        None,
        None,
        &project_a,
        cx,
    );

    let bounds_after = sidebar.read_with(cx, |sidebar, _| {
        sidebar
            .list_state
            .bounds_for_item(next_header_ix)
            .expect("same-shape metadata update should preserve next header measurements")
    });
    assert_eq!(bounds_before, bounds_after);
}

#[gpui::test]
async fn test_thread_status_update_does_not_reset_list_measurements(cx: &mut TestAppContext) {
    // When a thread's status changes (e.g. Running -> Completed after sending a message), the
    // shape sequence is unchanged, so `update_entries` should not reset the underlying
    // `ListState`. Resetting throws away measured item bounds for one frame, which makes the
    // sticky project header flicker between its pushed-off and fully-on-screen positions.
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_n_test_threads(2, &project, cx).await;
    cx.run_until_parked();

    let before = sidebar.read_with(cx, |sidebar, app| {
        sidebar
            .entry_shapes(multi_workspace.read(app))
            .collect::<Vec<_>>()
    });
    sidebar.update_in(cx, |sidebar, _window, cx| sidebar.update_entries(cx));
    cx.run_until_parked();
    let after = sidebar.read_with(cx, |sidebar, app| {
        sidebar
            .entry_shapes(multi_workspace.read(app))
            .collect::<Vec<_>>()
    });

    assert_eq!(
        before, after,
        "a no-op rebuild should produce an identical shape sequence"
    );
}

#[gpui::test]
async fn test_collapse_changes_entry_shape(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_n_test_threads(2, &project, cx).await;
    cx.run_until_parked();

    let project_group_key = project.read_with(cx, |project, cx| project.project_group_key(cx));

    let before = sidebar.read_with(cx, |sidebar, app| {
        sidebar
            .entry_shapes(multi_workspace.read(app))
            .collect::<Vec<_>>()
    });
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.toggle_collapse(&project_group_key, window, cx);
    });
    cx.run_until_parked();
    let after = sidebar.read_with(cx, |sidebar, app| {
        sidebar
            .entry_shapes(multi_workspace.read(app))
            .collect::<Vec<_>>()
    });

    assert_ne!(
        before, after,
        "collapsing the project group should change the shape sequence so the list resets"
    );
}

#[gpui::test]
async fn test_serialization_round_trip(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_n_test_threads(3, &project, cx).await;

    let project_group_key = project.read_with(cx, |project, cx| project.project_group_key(cx));

    // Set a custom width and collapse the group.
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.set_width(Some(px(420.0)), cx);
        sidebar.toggle_collapse(&project_group_key, window, cx);
    });
    cx.run_until_parked();

    // Capture the serialized state from the first sidebar.
    let serialized = sidebar.read_with(cx, |sidebar, cx| sidebar.serialized_state(cx));
    let serialized = serialized.expect("serialized_state should return Some");

    // Create a fresh sidebar and restore into it.
    let sidebar2 =
        cx.update(|window, cx| cx.new(|cx| Sidebar::new(multi_workspace.clone(), window, cx)));
    cx.run_until_parked();

    sidebar2.update_in(cx, |sidebar, window, cx| {
        sidebar.restore_serialized_state(&serialized, window, cx);
    });
    cx.run_until_parked();

    // Assert all serialized fields match.
    let width1 = sidebar.read_with(cx, |s, _| s.width);
    let width2 = sidebar2.read_with(cx, |s, _| s.width);

    assert_eq!(width1, width2);
    assert_eq!(width1, px(420.0));
}

#[gpui::test]
async fn test_width_reset_returns_configured_default(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    cx.update(|cx| set_threads_sidebar_default_width(360.0, cx));
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    assert_eq!(
        sidebar.read_with(cx, |sidebar, _| sidebar.width),
        px(360.0),
        "a fresh sidebar should open at the configured width"
    );

    sidebar.update_in(cx, |sidebar, _window, cx| {
        sidebar.set_width(Some(px(420.0)), cx);
    });
    cx.run_until_parked();
    assert_eq!(sidebar.read_with(cx, |sidebar, _| sidebar.width), px(420.0));

    sidebar.update_in(cx, |sidebar, _window, cx| {
        sidebar.set_width(None, cx);
    });
    cx.run_until_parked();

    assert_eq!(
        sidebar.read_with(cx, |sidebar, _| sidebar.width),
        px(360.0),
        "resetting the width should return to the configured width, not a fixed default"
    );
}

#[gpui::test]
async fn test_width_setting_overrides_manual_resize(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    cx.update(|_window, cx| set_threads_sidebar_default_width(360.0, cx));
    cx.run_until_parked();
    assert_eq!(sidebar.read_with(cx, |sidebar, _| sidebar.width), px(360.0));
    assert!(!sidebar.read_with(cx, |sidebar, _| sidebar.width_set_by_user));

    sidebar.update_in(cx, |sidebar, _window, cx| {
        sidebar.set_width(Some(px(420.0)), cx);
    });
    cx.update(|_window, cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store
                .set_user_settings(
                    r#"{"agent":{"threads_sidebar":{"default_width":360,"position":"right"}}}"#,
                    cx,
                )
                .expect("settings are valid");
        });
    });
    cx.run_until_parked();
    assert_eq!(sidebar.read_with(cx, |sidebar, _| sidebar.width), px(420.0));
    assert!(sidebar.read_with(cx, |sidebar, _| sidebar.width_set_by_user));

    cx.update(|_window, cx| set_threads_sidebar_default_width(500.0, cx));
    cx.run_until_parked();
    assert_eq!(sidebar.read_with(cx, |sidebar, _| sidebar.width), px(500.0));
    assert!(!sidebar.read_with(cx, |sidebar, _| sidebar.width_set_by_user));

    for (configured, expected) in [
        (5.0, THREADS_LIST_MIN_WIDTH),
        (360.0, px(360.0)),
        (5000.0, THREADS_LIST_MAX_WIDTH),
    ] {
        cx.update(|_window, cx| set_threads_sidebar_default_width(configured, cx));
        cx.run_until_parked();
        assert_eq!(sidebar.read_with(cx, |sidebar, _| sidebar.width), expected);
        let serialized = sidebar
            .read_with(cx, |sidebar, cx| sidebar.serialized_state(cx))
            .expect("sidebar state is serialized");
        let serialized: SerializedSidebar =
            serde_json::from_str(&serialized).expect("sidebar state is valid");
        assert_eq!(serialized.width, None);
        assert!(!serialized.width_set_by_user);
    }
}

#[gpui::test]
async fn test_restored_width_preserves_legacy_resizes(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));

    for (state, expected, width_set_by_user) in [
        (r#"{"width":null}"#, 360.0, false),
        (r#"{"width":300.0}"#, 360.0, false),
        (r#"{"width":420.0}"#, 420.0, true),
        (r#"{"width":300.0,"width_set_by_user":true}"#, 300.0, true),
    ] {
        cx.update(|_window, cx| set_threads_sidebar_default_width(360.0, cx));
        let sidebar =
            cx.update(|window, cx| cx.new(|cx| Sidebar::new(multi_workspace.clone(), window, cx)));
        cx.run_until_parked();
        sidebar.update_in(cx, |sidebar, window, cx| {
            sidebar.restore_serialized_state(state, window, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            sidebar.read_with(cx, |sidebar, _| sidebar.width),
            px(expected)
        );
        assert_eq!(
            sidebar.read_with(cx, |sidebar, _| sidebar.width_set_by_user),
            width_set_by_user
        );

        let serialized = sidebar
            .read_with(cx, |sidebar, cx| sidebar.serialized_state(cx))
            .expect("sidebar state is serialized");
        let serialized: SerializedSidebar =
            serde_json::from_str(&serialized).expect("sidebar state is valid");
        assert_eq!(serialized.width, width_set_by_user.then_some(expected));
        assert_eq!(serialized.width_set_by_user, width_set_by_user);

        cx.update(|_window, cx| set_threads_sidebar_default_width(500.0, cx));
        cx.run_until_parked();
        assert_eq!(sidebar.read_with(cx, |sidebar, _| sidebar.width), px(500.0));
        assert!(!sidebar.read_with(cx, |sidebar, _| sidebar.width_set_by_user));
    }
}

#[gpui::test]
async fn test_configured_width_is_clamped_into_range(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    cx.update(|cx| set_threads_sidebar_default_width(5000.0, cx));
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));

    let wide_sidebar =
        cx.update(|window, cx| cx.new(|cx| Sidebar::new(multi_workspace.clone(), window, cx)));
    cx.run_until_parked();
    assert_eq!(
        wide_sidebar.read_with(cx, |sidebar, _| sidebar.width),
        THREADS_LIST_MAX_WIDTH,
        "a configured width above the maximum should be clamped at construction"
    );

    wide_sidebar.update_in(cx, |sidebar, _window, cx| {
        sidebar.set_width(None, cx);
    });
    cx.run_until_parked();
    assert_eq!(
        wide_sidebar.read_with(cx, |sidebar, _| sidebar.width),
        THREADS_LIST_MAX_WIDTH,
        "resetting the width should not restore the unclamped configured width"
    );

    cx.update(|_window, cx| set_threads_sidebar_default_width(5.0, cx));
    let narrow_sidebar =
        cx.update(|window, cx| cx.new(|cx| Sidebar::new(multi_workspace.clone(), window, cx)));
    cx.run_until_parked();
    assert_eq!(
        narrow_sidebar.read_with(cx, |sidebar, _| sidebar.width),
        THREADS_LIST_MIN_WIDTH,
        "a configured width below the minimum should be clamped at construction"
    );
}

#[gpui::test]
async fn test_only_a_user_chosen_width_is_persisted(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    cx.update(|cx| set_threads_sidebar_default_width(360.0, cx));
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    // Serialization runs on many triggers besides resizing, so a sidebar the
    // user has never resized must not record a width at all.
    let untouched = sidebar
        .read_with(cx, |sidebar, cx| sidebar.serialized_state(cx))
        .expect("serialized_state should return Some");
    assert!(
        !untouched.contains("360"),
        "an unresized sidebar should not persist its width, got {untouched}"
    );

    // A legacy width matching the old default is ambiguous, so the setting takes precedence
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.restore_serialized_state(r#"{"width":300.0}"#, window, cx);
    });
    cx.run_until_parked();
    assert_eq!(
        sidebar.read_with(cx, |sidebar, _| sidebar.width),
        px(360.0),
        "the legacy default width falls back to the setting"
    );

    // A width the user picked survives, and keeps surviving across restores.
    sidebar.update_in(cx, |sidebar, _window, cx| {
        sidebar.set_width(Some(px(420.0)), cx);
    });
    cx.run_until_parked();
    let resized = sidebar
        .read_with(cx, |sidebar, cx| sidebar.serialized_state(cx))
        .expect("serialized_state should return Some");

    let restored =
        cx.update(|window, cx| cx.new(|cx| Sidebar::new(multi_workspace.clone(), window, cx)));
    cx.run_until_parked();
    restored.update_in(cx, |sidebar, window, cx| {
        sidebar.restore_serialized_state(&resized, window, cx);
    });
    cx.run_until_parked();
    assert_eq!(
        restored.read_with(cx, |sidebar, _| sidebar.width),
        px(420.0),
        "a user-chosen width should survive a restore"
    );

    // Resetting gives the width back to the setting, and stops persisting it.
    restored.update_in(cx, |sidebar, _window, cx| {
        sidebar.set_width(None, cx);
    });
    cx.run_until_parked();
    let after_reset = restored
        .read_with(cx, |sidebar, cx| sidebar.serialized_state(cx))
        .expect("serialized_state should return Some");
    assert!(
        !after_reset.contains("420"),
        "resetting should stop persisting the width, got {after_reset}"
    );
}

#[gpui::test]
async fn test_restore_serialized_archive_view_does_not_panic(cx: &mut TestAppContext) {
    // A regression test to ensure that restoring a serialized archive view does not panic.
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, _panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    cx.update(|_window, cx| {
        AgentRegistryStore::init_test_global(cx, vec![]);
    });

    let serialized = serde_json::to_string(&SerializedSidebar {
        width: Some(400.0),
        width_set_by_user: true,
        active_view: SerializedSidebarView::History,
    })
    .expect("serialization should succeed");

    multi_workspace.update_in(cx, |multi_workspace, window, cx| {
        if let Some(sidebar) = multi_workspace.sidebar() {
            sidebar.restore_serialized_state(&serialized, window, cx);
        }
    });
    cx.run_until_parked();

    // After the deferred `show_archive` runs, the view should be Archive.
    sidebar.read_with(cx, |sidebar, _cx| {
        assert!(
            matches!(sidebar.view, SidebarView::Archive(_)),
            "expected sidebar view to be Archive after restore, got ThreadList"
        );
    });
}

#[gpui::test]
async fn test_entities_released_on_window_close(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    let weak_workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().downgrade());
    let weak_sidebar = sidebar.downgrade();
    let weak_multi_workspace = multi_workspace.downgrade();

    drop(sidebar);
    drop(multi_workspace);
    cx.update(|window, _cx| window.remove_window());
    cx.run_until_parked();

    weak_multi_workspace.assert_released();
    weak_sidebar.assert_released();
    weak_workspace.assert_released();
}

#[gpui::test]
async fn test_single_workspace_no_threads(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let (_sidebar, _panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    assert_eq!(
        visible_entries_as_strings(&_sidebar, cx),
        vec!["v [my-project]"]
    );
}

#[gpui::test]
async fn test_single_workspace_with_saved_threads(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_thread_metadata(
        acp::SessionId::new(Arc::from("thread-1")),
        Some("Fix crash in project panel".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 3, 0, 0, 0).unwrap(),
        None,
        None,
        &project,
        cx,
    );

    save_thread_metadata(
        acp::SessionId::new(Arc::from("thread-2")),
        Some("Add inline diff view".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 2, 0, 0, 0).unwrap(),
        None,
        None,
        &project,
        cx,
    );
    cx.run_until_parked();

    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [my-project]",
            "  Fix crash in project panel",
            "  Add inline diff view",
        ]
    );
}

#[gpui::test]
async fn test_workspace_lifecycle(cx: &mut TestAppContext) {
    let project = init_test_project("/project-a", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    // Single workspace with a thread
    save_thread_metadata(
        acp::SessionId::new(Arc::from("thread-a1")),
        Some("Thread A1".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        None,
        None,
        &project,
        cx,
    );
    cx.run_until_parked();

    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [project-a]",
            "  Thread A1",
        ]
    );

    // Add a second workspace
    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.create_test_workspace(window, cx).detach();
    });
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [project-a]",
            "  Thread A1",
        ]
    );
}

#[gpui::test]
async fn test_collapse_and_expand_group(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_n_test_threads(1, &project, cx).await;

    let project_group_key = project.read_with(cx, |project, cx| project.project_group_key(cx));

    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [my-project]",
            "  Thread 1",
        ]
    );

    // Collapse
    sidebar.update_in(cx, |s, window, cx| {
        s.toggle_collapse(&project_group_key, window, cx);
    });
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "> [my-project]",
        ]
    );

    // Expand
    sidebar.update_in(cx, |s, window, cx| {
        s.toggle_collapse(&project_group_key, window, cx);
    });
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [my-project]",
            "  Thread 1",
        ]
    );
}

#[gpui::test]
async fn test_collapse_state_survives_worktree_key_change(cx: &mut TestAppContext) {
    // When a worktree is added to a project, the project group key changes.
    // The sidebar's collapsed/expanded state is keyed by ProjectGroupKey, so
    // UI state must survive the key change.
    let (_fs, project) = init_multi_project_test(&["/project-a", "/project-b"], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_n_test_threads(2, &project, cx).await;
    sidebar.update_in(cx, |sidebar, _window, cx| sidebar.update_entries(cx));
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["v [project-a]", "  Thread 2", "  Thread 1",]
    );

    // Collapse the group.
    let old_key = project.read_with(cx, |project, cx| project.project_group_key(cx));
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.toggle_collapse(&old_key, window, cx);
    });
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["> [project-a]"]
    );

    // Add a second worktree — the key changes from [/project-a] to
    // [/project-a, /project-b].
    project
        .update(cx, |project, cx| {
            project.find_or_create_worktree("/project-b", true, cx)
        })
        .await
        .expect("should add worktree");
    cx.run_until_parked();

    sidebar.update_in(cx, |sidebar, _window, cx| sidebar.update_entries(cx));
    cx.run_until_parked();

    // The group should still be collapsed under the new key.
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["> [project-a, project-b]"]
    );
}

#[gpui::test]
async fn test_neighboring_activatable_entry_stays_within_project(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

    let header = |path: &str| ListEntry::ProjectHeader {
        key: ProjectGroupKey::new(None, PathList::new(&[std::path::PathBuf::from(path)])),
        label: path.into(),
        highlight_positions: Vec::new(),
        has_running_threads: false,
        waiting_thread_count: 0,
        has_notifications: false,
        is_active: false,
        has_threads: true,
    };
    let thread = |name: &str| {
        ListEntry::Thread(Arc::new(ThreadEntry {
            metadata: ThreadMetadata {
                thread_id: ThreadId::new(),
                session_id: Some(acp::SessionId::new(Arc::from(name))),
                agent_id: AgentId::new("zed-agent"),
                worktree_paths: WorktreePaths::default(),
                title: Some(name.to_string().into()),
                title_override: None,
                updated_at: Utc::now(),
                created_at: Some(Utc::now()),
                interacted_at: None,
                archived: false,
                remote_connection: None,
            },
            icon: IconName::ZedAgent,
            icon_from_external_svg: None,
            status: AgentThreadStatus::Completed,
            workspace: ThreadEntryWorkspace::Open(workspace.clone()),
            is_live: false,
            is_background: false,
            is_title_generating: false,
            draft: None,
            highlight_positions: Vec::new(),
            worktrees: Vec::new(),
            diff_stats: DiffStats::default(),
        }))
    };

    sidebar.update_in(cx, |s, _window, _cx| {
        s.contents.entries = vec![
            header("/project-a"),
            thread("a-newest"),
            thread("a-oldest"),
            header("/project-b"),
            thread("b-newest"),
        ];

        let neighbor_session = |position: usize| match s.neighboring_activatable_entry(position) {
            Some(ActivatableEntry::Thread { metadata, .. }) => metadata.session_id,
            _ => None,
        };

        assert_eq!(
            neighbor_session(2),
            Some(acp::SessionId::new(Arc::from("a-newest"))),
            "the neighbor should be the sibling in the same project, not the next project's thread"
        );
        assert_eq!(
            neighbor_session(4),
            Some(acp::SessionId::new(Arc::from("a-oldest"))),
            "an empty project should fall back to another project"
        );
    });
}

#[gpui::test]
async fn test_visible_entries_as_strings(cx: &mut TestAppContext) {
    use workspace::ProjectGroup;

    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
    let expanded_path = PathList::new(&[std::path::PathBuf::from("/expanded")]);
    let collapsed_path = PathList::new(&[std::path::PathBuf::from("/collapsed")]);

    // Set the collapsed group state through multi_workspace
    multi_workspace.update(cx, |mw, _cx| {
        mw.test_add_project_group(ProjectGroup {
            key: ProjectGroupKey::new(None, collapsed_path.clone()),
            workspaces: Vec::new(),
            expanded: false,
        });
    });

    sidebar.update_in(cx, |s, _window, _cx| {
        let notified_thread_id = ThreadId::new();
        s.contents.notified_threads.insert(notified_thread_id);
        s.contents.entries = vec![
            // Expanded project header
            ListEntry::ProjectHeader {
                key: ProjectGroupKey::new(None, expanded_path.clone()),
                label: "expanded-project".into(),
                highlight_positions: Vec::new(),
                has_running_threads: false,
                waiting_thread_count: 0,
                has_notifications: false,
                is_active: true,
                has_threads: true,
            },
            ListEntry::Thread(Arc::new(ThreadEntry {
                metadata: ThreadMetadata {
                    thread_id: ThreadId::new(),
                    session_id: Some(acp::SessionId::new(Arc::from("t-1"))),
                    agent_id: AgentId::new("zed-agent"),
                    worktree_paths: WorktreePaths::default(),
                    title: Some("Completed thread".into()),
                    title_override: None,
                    updated_at: Utc::now(),
                    created_at: Some(Utc::now()),
                    interacted_at: None,
                    archived: false,
                    remote_connection: None,
                },
                icon: IconName::ZedAgent,
                icon_from_external_svg: None,
                status: AgentThreadStatus::Completed,
                workspace: ThreadEntryWorkspace::Open(workspace.clone()),
                is_live: false,
                is_background: false,
                is_title_generating: false,
                draft: None,
                highlight_positions: Vec::new(),
                worktrees: Vec::new(),
                diff_stats: DiffStats::default(),
            })),
            // Active thread with Running status
            ListEntry::Thread(Arc::new(ThreadEntry {
                metadata: ThreadMetadata {
                    thread_id: ThreadId::new(),
                    session_id: Some(acp::SessionId::new(Arc::from("t-2"))),
                    agent_id: AgentId::new("zed-agent"),
                    worktree_paths: WorktreePaths::default(),
                    title: Some("Running thread".into()),
                    title_override: None,
                    updated_at: Utc::now(),
                    created_at: Some(Utc::now()),
                    interacted_at: None,
                    archived: false,
                    remote_connection: None,
                },
                icon: IconName::ZedAgent,
                icon_from_external_svg: None,
                status: AgentThreadStatus::Running,
                workspace: ThreadEntryWorkspace::Open(workspace.clone()),
                is_live: true,
                is_background: false,
                is_title_generating: false,
                draft: None,
                highlight_positions: Vec::new(),
                worktrees: Vec::new(),
                diff_stats: DiffStats::default(),
            })),
            // Active thread with Error status
            ListEntry::Thread(Arc::new(ThreadEntry {
                metadata: ThreadMetadata {
                    thread_id: ThreadId::new(),
                    session_id: Some(acp::SessionId::new(Arc::from("t-3"))),
                    agent_id: AgentId::new("zed-agent"),
                    worktree_paths: WorktreePaths::default(),
                    title: Some("Error thread".into()),
                    title_override: None,
                    updated_at: Utc::now(),
                    created_at: Some(Utc::now()),
                    interacted_at: None,
                    archived: false,
                    remote_connection: None,
                },
                icon: IconName::ZedAgent,
                icon_from_external_svg: None,
                status: AgentThreadStatus::Error,
                workspace: ThreadEntryWorkspace::Open(workspace.clone()),
                is_live: true,
                is_background: false,
                is_title_generating: false,
                draft: None,
                highlight_positions: Vec::new(),
                worktrees: Vec::new(),
                diff_stats: DiffStats::default(),
            })),
            // Thread with WaitingForConfirmation status, not active
            // remote_connection: None,
            ListEntry::Thread(Arc::new(ThreadEntry {
                metadata: ThreadMetadata {
                    thread_id: ThreadId::new(),
                    session_id: Some(acp::SessionId::new(Arc::from("t-4"))),
                    agent_id: AgentId::new("zed-agent"),
                    worktree_paths: WorktreePaths::default(),
                    title: Some("Waiting thread".into()),
                    title_override: None,
                    updated_at: Utc::now(),
                    created_at: Some(Utc::now()),
                    interacted_at: None,
                    archived: false,
                    remote_connection: None,
                },
                icon: IconName::ZedAgent,
                icon_from_external_svg: None,
                status: AgentThreadStatus::WaitingForConfirmation,
                workspace: ThreadEntryWorkspace::Open(workspace.clone()),
                is_live: false,
                is_background: false,
                is_title_generating: false,
                draft: None,
                highlight_positions: Vec::new(),
                worktrees: Vec::new(),
                diff_stats: DiffStats::default(),
            })),
            // Background thread that completed (should show notification)
            // remote_connection: None,
            ListEntry::Thread(Arc::new(ThreadEntry {
                metadata: ThreadMetadata {
                    thread_id: notified_thread_id,
                    session_id: Some(acp::SessionId::new(Arc::from("t-5"))),
                    agent_id: AgentId::new("zed-agent"),
                    worktree_paths: WorktreePaths::default(),
                    title: Some("Notified thread".into()),
                    title_override: None,
                    updated_at: Utc::now(),
                    created_at: Some(Utc::now()),
                    interacted_at: None,
                    archived: false,
                    remote_connection: None,
                },
                icon: IconName::ZedAgent,
                icon_from_external_svg: None,
                status: AgentThreadStatus::Completed,
                workspace: ThreadEntryWorkspace::Open(workspace.clone()),
                is_live: true,
                is_background: true,
                is_title_generating: false,
                draft: None,
                highlight_positions: Vec::new(),
                worktrees: Vec::new(),
                diff_stats: DiffStats::default(),
            })),
            // Collapsed project header
            ListEntry::ProjectHeader {
                key: ProjectGroupKey::new(None, collapsed_path.clone()),
                label: "collapsed-project".into(),
                highlight_positions: Vec::new(),
                has_running_threads: false,
                waiting_thread_count: 0,
                has_notifications: false,
                is_active: false,
                has_threads: false,
            },
        ];

        // Select the Running thread (index 2)
        s.selection = Some(2);
    });

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [expanded-project]",
            "  Completed thread",
            "  Running thread * (running)  <== selected",
            "  Error thread * (error)",
            "  Waiting thread (waiting)",
            "  Notified thread * (!)",
            "> [collapsed-project]",
        ]
    );

    // Move selection to the collapsed header
    sidebar.update_in(cx, |s, _window, _cx| {
        s.selection = Some(6);
    });

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx).last().cloned(),
        Some("> [collapsed-project]  <== selected".to_string()),
    );

    // Clear selection
    sidebar.update_in(cx, |s, _window, _cx| {
        s.selection = None;
    });

    // No entry should have the selected marker
    let entries = visible_entries_as_strings(&sidebar, cx);
    for entry in &entries {
        assert!(
            !entry.contains("<== selected"),
            "unexpected selection marker in: {}",
            entry
        );
    }
}

#[gpui::test]
async fn test_keyboard_select_next_and_previous(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_n_test_threads(3, &project, cx).await;

    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    // Entries: [header, thread3, thread2, thread1]
    // Focusing the sidebar does not set a selection; select_next/select_previous
    // handle None gracefully by starting from the first or last entry.
    focus_sidebar(&sidebar, cx);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), None);

    // First SelectNext from None starts at index 0
    cx.dispatch_action(SelectNext);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), Some(0));

    // Move down through remaining entries
    cx.dispatch_action(SelectNext);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), Some(1));

    cx.dispatch_action(SelectNext);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), Some(2));

    cx.dispatch_action(SelectNext);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), Some(3));

    // At the end, wraps back to first entry
    cx.dispatch_action(SelectNext);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), Some(0));

    // Navigate back to the end
    cx.dispatch_action(SelectNext);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), Some(1));
    cx.dispatch_action(SelectNext);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), Some(2));
    cx.dispatch_action(SelectNext);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), Some(3));

    // Move back up
    cx.dispatch_action(SelectPrevious);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), Some(2));

    cx.dispatch_action(SelectPrevious);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), Some(1));

    cx.dispatch_action(SelectPrevious);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), Some(0));

    // At the top, selection clears (focus returns to editor)
    cx.dispatch_action(SelectPrevious);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), None);
}

#[gpui::test]
async fn test_keyboard_select_first_and_last(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_n_test_threads(3, &project, cx).await;
    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    focus_sidebar(&sidebar, cx);

    // SelectLast jumps to the end
    cx.dispatch_action(SelectLast);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), Some(3));

    // SelectFirst jumps to the beginning
    cx.dispatch_action(SelectFirst);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), Some(0));
}

#[gpui::test]
async fn test_refocus_sidebar_with_no_selection_focuses_search(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    let workspace = multi_workspace.read_with(cx, |multi_workspace, _cx| {
        multi_workspace.workspace().clone()
    });

    workspace.update_in(cx, |workspace, window, cx| {
        workspace.focus_handle(cx).focus(window, cx);
    });
    cx.run_until_parked();
    sidebar.update_in(cx, |sidebar, window, cx| {
        assert!(!sidebar.focus_handle.is_focused(window));
        assert!(!sidebar.filter_editor.read(cx).is_focused(window));
        assert_eq!(sidebar.selection, None);
    });

    // Refocusing with no selection sends focus to search without selecting a row.
    focus_sidebar(&sidebar, cx);
    sidebar.update_in(cx, |sidebar, window, cx| {
        assert!(sidebar.filter_editor.read(cx).is_focused(window));
        assert_eq!(sidebar.selection, None);
    });
}

#[gpui::test]
async fn test_refocus_sidebar_with_selection_preserves_it(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    let workspace = multi_workspace.read_with(cx, |multi_workspace, _cx| {
        multi_workspace.workspace().clone()
    });

    sidebar.update(cx, |sidebar, cx| {
        sidebar.selection = Some(0);
        cx.notify();
    });
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.focus_handle(cx).focus(window, cx);
    });
    cx.run_until_parked();

    // Refocusing with a selection returns focus to the list and keeps that row selected.
    focus_sidebar(&sidebar, cx);
    sidebar.update_in(cx, |sidebar, window, _cx| {
        assert!(sidebar.focus_handle.is_focused(window));
        assert_eq!(sidebar.selection, Some(0));
    });
}

#[gpui::test]
async fn test_keyboard_confirm_on_project_header_toggles_collapse(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_n_test_threads(1, &project, cx).await;
    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [my-project]",
            "  Thread 1",
        ]
    );

    // Focus the sidebar and select the header
    focus_sidebar(&sidebar, cx);
    sidebar.update_in(cx, |sidebar, _window, _cx| {
        sidebar.selection = Some(0);
    });

    // Confirm on project header collapses the group
    cx.dispatch_action(Confirm);
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "> [my-project]  <== selected",
        ]
    );

    // Confirm again expands the group
    cx.dispatch_action(Confirm);
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [my-project]  <== selected",
            "  Thread 1",
        ]
    );
}

#[gpui::test]
async fn test_keyboard_expand_and_collapse_selected_entry(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_n_test_threads(1, &project, cx).await;
    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [my-project]",
            "  Thread 1",
        ]
    );

    // Focus sidebar and manually select the header (index 0). Press left to collapse.
    focus_sidebar(&sidebar, cx);
    sidebar.update_in(cx, |sidebar, _window, _cx| {
        sidebar.selection = Some(0);
    });

    cx.dispatch_action(SelectParent);
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "> [my-project]  <== selected",
        ]
    );

    // Press right to expand
    cx.dispatch_action(SelectChild);
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [my-project]  <== selected",
            "  Thread 1",
        ]
    );

    // Press right again on already-expanded header moves selection down
    cx.dispatch_action(SelectChild);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), Some(1));
}

#[gpui::test]
async fn test_keyboard_collapse_from_child_selects_parent(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_n_test_threads(1, &project, cx).await;
    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    // Focus sidebar (selection starts at None), then navigate down to the thread (child)
    focus_sidebar(&sidebar, cx);
    cx.dispatch_action(SelectNext);
    cx.dispatch_action(SelectNext);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), Some(1));

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [my-project]",
            "  Thread 1  <== selected",
        ]
    );

    // Pressing left on a child collapses the parent group and selects it
    cx.dispatch_action(SelectParent);
    cx.run_until_parked();

    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), Some(0));
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "> [my-project]  <== selected",
        ]
    );
}

#[gpui::test]
async fn test_keyboard_navigation_on_empty_list(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/empty-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let (sidebar, _panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    // An empty project has only the header (no auto-created draft).
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["v [empty-project]"]
    );

    // Focus sidebar — focus_in does not set a selection
    focus_sidebar(&sidebar, cx);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), None);

    // First SelectNext from None starts at index 0 (header)
    cx.dispatch_action(SelectNext);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), Some(0));

    // SelectNext with only one entry stays at index 0
    cx.dispatch_action(SelectNext);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), Some(0));

    // SelectPrevious from first entry clears selection (returns to editor)
    cx.dispatch_action(SelectPrevious);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), None);

    // SelectPrevious from None selects the last entry
    cx.dispatch_action(SelectPrevious);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), Some(0));
}

#[gpui::test]
async fn test_new_entry_noops_without_open_project(cx: &mut TestAppContext) {
    init_test(cx);

    let fs = FakeFs::new(cx.executor());
    cx.update(|cx| <dyn Fs>::set_global(fs.clone(), cx));
    let project = project::Project::test(fs, [], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    let workspace = multi_workspace.read_with(cx, |multi_workspace, _cx| {
        multi_workspace.workspace().clone()
    });

    assert!(
        !sidebar.read_with(cx, |sidebar, _cx| sidebar.contents.has_open_projects),
        "empty workspaces should be treated as having no open projects"
    );

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.create_new_entry(&workspace, window, cx);
    });
    cx.run_until_parked();

    panel.read_with(cx, |panel, _cx| {
        assert!(
            panel.active_conversation_view().is_none(),
            "sidebar should not create an agent thread without an open project"
        );
    });
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        Vec::<String>::new()
    );
}

#[gpui::test]
async fn test_selection_clamps_after_entry_removal(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_n_test_threads(1, &project, cx).await;
    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    // Focus sidebar (selection starts at None), navigate down to the thread (index 1)
    focus_sidebar(&sidebar, cx);
    cx.dispatch_action(SelectNext);
    cx.dispatch_action(SelectNext);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), Some(1));

    // Collapse the group, which removes the thread from the list
    cx.dispatch_action(SelectParent);
    cx.run_until_parked();

    // Selection should be clamped to the last valid index (0 = header)
    let selection = sidebar.read_with(cx, |s, _| s.selection);
    let entry_count = sidebar.read_with(cx, |s, _| s.contents.entries.len());
    assert!(
        selection.unwrap_or(0) < entry_count,
        "selection {} should be within bounds (entries: {})",
        selection.unwrap_or(0),
        entry_count,
    );
}

async fn init_test_project_with_agent_panel(
    worktree_path: &str,
    cx: &mut TestAppContext,
) -> Entity<project::Project> {
    use_unique_metadata_databases(cx);
    agent_ui::test_support::init_test(cx);
    cx.update(|cx| {
        set_max_idle_retained_threads(1, cx);
        ThreadStore::init_global(cx);
        ThreadMetadataStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
        prompt_store::init(cx);
    });

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(worktree_path, serde_json::json!({ "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
    project::Project::test(fs, [worktree_path.as_ref()], cx).await
}

fn add_agent_panel(
    workspace: &Entity<Workspace>,
    cx: &mut gpui::VisualTestContext,
) -> Entity<AgentPanel> {
    workspace.update_in(cx, |workspace, window, cx| {
        let panel = cx.new(|cx| AgentPanel::test_new(workspace, window, cx));
        workspace.add_panel(panel.clone(), window, cx);
        panel
    })
}

fn setup_sidebar_with_agent_panel(
    multi_workspace: &Entity<MultiWorkspace>,
    cx: &mut gpui::VisualTestContext,
) -> (Entity<Sidebar>, Entity<AgentPanel>) {
    let sidebar = setup_sidebar(multi_workspace, cx);
    let workspace = multi_workspace.read_with(cx, |mw, _cx| mw.workspace().clone());
    let panel = add_agent_panel(&workspace, cx);
    (sidebar, panel)
}

#[gpui::test]
async fn test_agent_panel_terminals_appear_in_sidebar_and_search(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    let terminal_id = panel
        .update_in(cx, |panel, window, cx| {
            panel.insert_test_terminal("Dev Server", true, window, cx)
        })
        .expect("test terminal should be inserted");
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["v [my-project]", "  Dev Server"]
    );
    sidebar.read_with(cx, |sidebar, _cx| {
        assert!(
            matches!(&sidebar.active_entry, Some(ActiveEntry::Terminal { terminal_id: active_terminal_id, .. }) if *active_terminal_id == terminal_id),
            "expected active terminal entry, got {:?}",
            sidebar.active_entry,
        );
        assert!(
            sidebar.contents.entries.iter().any(|entry| {
                matches!(entry, ListEntry::Terminal(terminal) if terminal.metadata.terminal_id == terminal_id && terminal.metadata.display_title().as_ref() == "Dev Server")
            }),
            "expected the inserted terminal to appear in sidebar contents",
        );
    });
    sidebar.read_with(cx, |_sidebar, cx| {
        let store = TerminalThreadMetadataStore::global(cx).read(cx);
        let metadata = store
            .entry(terminal_id)
            .expect("terminal metadata should be persisted");
        assert_eq!(metadata.title.as_ref(), "");
        assert_eq!(
            metadata.custom_title.as_ref().map(|title| title.as_ref()),
            Some("Dev Server")
        );
        assert_eq!(metadata.display_title().as_ref(), "Dev Server");
        assert!(
            metadata
                .folder_paths()
                .paths()
                .iter()
                .any(|path| path.as_path() == Path::new("/my-project"))
        );
    });

    type_in_search(&sidebar, "server", cx);
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["v [my-project]", "  Dev Server  <== selected"]
    );

    type_in_search(&sidebar, "missing", cx);
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        Vec::<String>::new()
    );
}

#[gpui::test]
async fn test_closing_last_agent_panel_terminal_restores_empty_header(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    assert_project_header_has_threads(&sidebar, "my-project", false, cx);

    let terminal_id = panel
        .update_in(cx, |panel, window, cx| {
            panel.insert_test_terminal("Dev Server", true, window, cx)
        })
        .expect("test terminal should be inserted");
    cx.run_until_parked();

    assert_project_header_has_threads(&sidebar, "my-project", true, cx);

    let (terminal_metadata, terminal_workspace) = sidebar.read_with(cx, |sidebar, _cx| {
        sidebar
            .contents
            .entries
            .iter()
            .find_map(|entry| match entry {
                ListEntry::Terminal(terminal) if terminal.metadata.terminal_id == terminal_id => {
                    Some((terminal.metadata.clone(), terminal.workspace.clone()))
                }
                _ => None,
            })
            .expect("terminal should be visible in sidebar")
    });
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.close_terminal(&terminal_metadata, &terminal_workspace, window, cx);
    });
    cx.run_until_parked();

    panel.read_with(cx, |panel, cx| {
        assert!(!panel.has_terminal(terminal_id));
        assert!(
            panel.active_view_is_new_draft(cx),
            "closing the active terminal should leave the panel on its empty draft"
        );
    });
    // Closing the terminal drops the user back onto the panel's empty
    // draft. The sidebar mirrors that with a "New {agent} Thread"
    // placeholder row, so the header reports having threads.
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["v [my-project]", "  New Zed Agent Thread"]
    );
    assert_project_header_has_threads(&sidebar, "my-project", true, cx);

    let project_group_key = multi_workspace.read_with(cx, |multi_workspace, cx| {
        multi_workspace.workspace().read(cx).project_group_key(cx)
    });
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.toggle_collapse(&project_group_key, window, cx);
    });
    cx.run_until_parked();

    // Collapsed: header hides children but still reports the placeholder
    // as a thread present in the group.
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["> [my-project]"]
    );
    assert_project_header_has_threads(&sidebar, "my-project", true, cx);
}

#[gpui::test]
async fn test_agent_panel_terminal_metadata_remains_visible_after_panel_is_removed(
    cx: &mut TestAppContext,
) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    let workspace = multi_workspace.read_with(cx, |multi_workspace, _cx| {
        multi_workspace.workspace().clone()
    });

    let terminal_id = panel
        .update_in(cx, |panel, window, cx| {
            panel.insert_test_terminal("Dev Server", true, window, cx)
        })
        .expect("test terminal should be inserted");
    cx.run_until_parked();

    workspace.update_in(cx, |workspace, window, cx| {
        workspace.remove_panel(&panel, window, cx);
    });
    sidebar.update(cx, |sidebar, cx| sidebar.update_entries(cx));
    cx.run_until_parked();

    assert!(workspace.read_with(cx, |workspace, cx| {
        workspace.panel::<AgentPanel>(cx).is_none()
    }));
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["v [my-project]", "  Dev Server"]
    );

    sidebar.read_with(cx, |sidebar, _cx| {
        assert!(sidebar.contents.entries.iter().any(|entry| {
            matches!(entry, ListEntry::Terminal(terminal) if terminal.metadata.terminal_id == terminal_id)
        }));
    });
}

#[gpui::test]
async fn test_terminal_metadata_is_deduped_across_project_groups(cx: &mut TestAppContext) {
    agent_ui::test_support::init_test(cx);
    cx.update(|cx| {
        set_max_idle_retained_threads(1, cx);
        ThreadStore::init_global(cx);
        ThreadMetadataStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
        prompt_store::init(cx);
    });

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/project-a", serde_json::json!({ "src": {} }))
        .await;
    fs.insert_tree("/project-b", serde_json::json!({ "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let project_a = project::Project::test(fs.clone(), ["/project-a".as_ref()], cx).await;
    let project_b = project::Project::test(fs.clone(), ["/project-b".as_ref()], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a, window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    let workspace_a = multi_workspace.read_with(cx, |multi_workspace, _cx| {
        multi_workspace.workspace().clone()
    });
    multi_workspace.update_in(cx, |multi_workspace, window, cx| {
        multi_workspace.test_add_workspace(project_b, window, cx);
    });
    let terminal_id = panel
        .update_in(cx, |panel, window, cx| {
            panel.insert_test_terminal("Original", true, window, cx)
        })
        .expect("test terminal should be inserted");
    cx.run_until_parked();

    workspace_a.update_in(cx, |workspace, window, cx| {
        workspace.remove_panel(&panel, window, cx);
    });
    let now = Utc::now();
    let metadata = TerminalThreadMetadata {
        terminal_id,
        title: "Dev Server".into(),
        custom_title: None,
        created_at: now,
        worktree_paths: WorktreePaths::from_path_lists(
            PathList::new(&[PathBuf::from("/project-a")]),
            PathList::new(&[PathBuf::from("/project-b")]),
        )
        .unwrap(),
        remote_connection: None,
        working_directory: None,
    };

    cx.update(|_, cx| {
        TerminalThreadMetadataStore::global(cx).update(cx, |store, cx| {
            store.save(metadata, cx);
        });
    });
    sidebar.update(cx, |sidebar, cx| sidebar.update_entries(cx));
    cx.run_until_parked();

    sidebar.read_with(cx, |sidebar, _cx| {
        assert_eq!(
            sidebar
                .contents
                .entries
                .iter()
                .filter(|entry| {
                    matches!(
                        entry,
                        ListEntry::Terminal(terminal)
                            if terminal.metadata.terminal_id == terminal_id
                    )
                })
                .count(),
            1
        );
    });
}
#[gpui::test]
async fn test_terminal_close_event_closes_sidebar_terminal(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    let terminal_id = panel
        .update_in(cx, |panel, window, cx| {
            panel.insert_test_terminal("Dev Server", true, window, cx)
        })
        .expect("test terminal should be inserted");
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["v [my-project]", "  Dev Server"]
    );

    panel.update(cx, |panel, cx| {
        panel.emit_test_terminal_close(terminal_id, cx);
    });
    cx.run_until_parked();

    panel.read_with(cx, |panel, _cx| {
        assert!(!panel.has_terminal(terminal_id));
    });
    sidebar.read_with(cx, |sidebar, _cx| {
        assert!(sidebar.contents.entries.iter().all(|entry| {
            !matches!(entry, ListEntry::Terminal(terminal) if terminal.metadata.terminal_id == terminal_id)
        }));
    });
    sidebar.read_with(cx, |_sidebar, cx| {
        assert!(
            TerminalThreadMetadataStore::global(cx)
                .read(cx)
                .entry(terminal_id)
                .is_none(),
            "terminal metadata should be deleted when the terminal requests close"
        );
    });
}

#[gpui::test]
async fn test_terminal_close_event_activates_neighbor(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    let build_terminal_id = panel
        .update_in(cx, |panel, window, cx| {
            panel.insert_test_terminal("Build", true, window, cx)
        })
        .expect("build test terminal should be inserted");
    let server_terminal_id = panel
        .update_in(cx, |panel, window, cx| {
            panel.insert_test_terminal("Server", true, window, cx)
        })
        .expect("server test terminal should be inserted");
    cx.run_until_parked();

    panel.update(cx, |panel, cx| {
        panel.emit_test_terminal_close(server_terminal_id, cx);
    });
    cx.run_until_parked();

    panel.read_with(cx, |panel, _cx| {
        assert!(!panel.has_terminal(server_terminal_id));
        assert_eq!(panel.active_terminal_id(), Some(build_terminal_id));
    });
    sidebar.read_with(cx, |sidebar, _cx| {
        assert!(
            matches!(&sidebar.active_entry, Some(ActiveEntry::Terminal { terminal_id, .. }) if *terminal_id == build_terminal_id),
            "expected remaining terminal to become active, got {:?}",
            sidebar.active_entry,
        );
    });
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["v [my-project]", "  Build"]
    );
}

#[gpui::test]
async fn test_agent_panel_terminal_notifications_update_sidebar(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    let build_terminal_id = panel
        .update_in(cx, |panel, window, cx| {
            panel.insert_test_terminal("Build", true, window, cx)
        })
        .expect("build test terminal should be inserted");
    let server_terminal_id = panel
        .update_in(cx, |panel, window, cx| {
            panel.insert_test_terminal("Server", true, window, cx)
        })
        .expect("server test terminal should be inserted");
    cx.run_until_parked();

    panel.read_with(cx, |panel, _cx| {
        assert_eq!(panel.active_terminal_id(), Some(server_terminal_id));
    });

    panel.update(cx, |panel, cx| {
        panel.emit_test_terminal_bell(build_terminal_id, cx);
    });
    cx.run_until_parked();

    sidebar.read_with(cx, |sidebar, cx| {
        assert!(sidebar.has_notifications(cx));
        assert!(sidebar.contents.notified_terminals.contains(&build_terminal_id));
        assert!(sidebar.contents.entries.iter().any(|entry| {
            matches!(entry, ListEntry::Terminal(terminal) if terminal.metadata.terminal_id == build_terminal_id && terminal.has_notification)
        }));
    });

    panel.update_in(cx, |panel, window, cx| {
        panel.activate_terminal(build_terminal_id, true, window, cx);
    });
    cx.run_until_parked();

    sidebar.read_with(cx, |sidebar, cx| {
        assert!(!sidebar.has_notifications(cx));
        assert!(
            !sidebar
                .contents
                .notified_terminals
                .contains(&build_terminal_id)
        );
    });
}

#[gpui::test]
async fn test_thread_switcher_can_activate_agent_panel_terminal(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    let build_terminal_id = panel
        .update_in(cx, |panel, window, cx| {
            panel.insert_test_terminal("Build", true, window, cx)
        })
        .expect("build test terminal should be inserted");
    let server_terminal_id = panel
        .update_in(cx, |panel, window, cx| {
            panel.insert_test_terminal("Server", true, window, cx)
        })
        .expect("server test terminal should be inserted");
    cx.run_until_parked();

    focus_sidebar(&sidebar, cx);
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.on_toggle_thread_switcher(&ToggleThreadSwitcher::default(), window, cx);
    });
    cx.run_until_parked();

    let (entry_terminal_ids, selected_terminal_id) = sidebar.read_with(cx, |sidebar, cx| {
        let switcher = sidebar
            .thread_switcher
            .as_ref()
            .expect("switcher should be open");
        let switcher = switcher.read(cx);
        let entry_terminal_ids = switcher
            .entries()
            .iter()
            .map(|entry| {
                entry
                    .terminal_id()
                    .expect("expected terminal switcher entry")
            })
            .collect::<Vec<_>>();
        let selected_terminal_id = switcher
            .selected_entry()
            .expect("switcher should have selected entry")
            .terminal_id()
            .expect("expected selected terminal switcher entry");
        (entry_terminal_ids, selected_terminal_id)
    });

    assert_eq!(entry_terminal_ids.len(), 2);
    assert!(entry_terminal_ids.contains(&build_terminal_id));
    assert!(entry_terminal_ids.contains(&server_terminal_id));

    sidebar.update_in(cx, |sidebar, window, cx| {
        let switcher = sidebar
            .thread_switcher
            .as_ref()
            .expect("switcher should be open");
        let focus = switcher.focus_handle(cx);
        focus.dispatch_action(&menu::Confirm, window, cx);
    });
    cx.run_until_parked();

    panel.read_with(cx, |panel, _cx| {
        assert_eq!(panel.active_terminal_id(), Some(selected_terminal_id));
    });
    sidebar.read_with(cx, |sidebar, _cx| {
        assert!(
            matches!(&sidebar.active_entry, Some(ActiveEntry::Terminal { terminal_id, .. }) if *terminal_id == selected_terminal_id),
            "expected selected terminal to become active, got {:?}",
            sidebar.active_entry,
        );
    });
}

#[gpui::test]
async fn test_thread_switcher_includes_terminal_metadata_for_open_project_group(
    cx: &mut TestAppContext,
) {
    let project = init_test_project_with_agent_panel("/project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    let terminal_id = panel
        .update_in(cx, |panel, window, cx| {
            panel.insert_test_terminal("Feature Terminal", true, window, cx)
        })
        .expect("test terminal should be inserted");
    panel.update_in(cx, |panel, window, cx| {
        panel.close_terminal(terminal_id, window, cx);
    });
    save_thread_metadata(
        acp::SessionId::new(Arc::from("thread-newer")),
        Some("Newer Thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 3, 0, 0, 0).unwrap(),
        None,
        None,
        &project,
        cx,
    );
    save_thread_metadata(
        acp::SessionId::new(Arc::from("thread-older")),
        Some("Older Thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 2, 0, 0, 0).unwrap(),
        None,
        None,
        &project,
        cx,
    );

    let created_at = chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap();
    let metadata = TerminalThreadMetadata {
        terminal_id,
        title: "Feature Terminal".into(),
        custom_title: None,
        created_at,
        worktree_paths: WorktreePaths::from_path_lists(
            PathList::new(&[PathBuf::from("/project")]),
            PathList::new(&[PathBuf::from("/project-feature")]),
        )
        .unwrap(),
        remote_connection: None,
        working_directory: None,
    };
    cx.update(|_, cx| {
        TerminalThreadMetadataStore::global(cx).update(cx, |store, cx| {
            store.save(metadata, cx);
        });
    });
    sidebar.update(cx, |sidebar, cx| sidebar.update_entries(cx));
    cx.run_until_parked();

    focus_sidebar(&sidebar, cx);
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.on_toggle_thread_switcher(&ToggleThreadSwitcher::default(), window, cx);
    });
    cx.run_until_parked();

    sidebar.read_with(cx, |sidebar, cx| {
        let switcher = sidebar
            .thread_switcher
            .as_ref()
            .expect("switcher should be open");
        assert!(
            switcher
                .read(cx)
                .entries()
                .iter()
                .any(|entry| entry.terminal_id() == Some(terminal_id)),
            "terminal metadata row should be included like a closed thread row"
        );
    });
}
#[gpui::test]
async fn test_archive_thread_with_deleted_closed_linked_worktree(cx: &mut TestAppContext) {
    let (main_project, _fs) = init_test_project_with_git("/project", cx).await;
    main_project
        .update(cx, |project, cx| project.wait_for_initial_scan(cx))
        .await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    let main_workspace =
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());

    // A thread on a linked worktree that no longer exists on disk.
    let worktree_session_id = acp::SessionId::new(Arc::from("worktree-thread"));
    let worktree_folder_paths =
        PathList::new(&[PathBuf::from("/worktrees/project/feature-a/project")]);
    save_thread_metadata_with_main_paths(
        "worktree-thread",
        "Worktree Thread",
        worktree_folder_paths,
        PathList::new(&[PathBuf::from("/project")]),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        cx,
    );
    sidebar.update(cx, |sidebar, cx| sidebar.update_entries(cx));
    cx.run_until_parked();

    sidebar.read_with(cx, |sidebar, _cx| {
        let thread = sidebar
            .contents
            .entries
            .iter()
            .find_map(|entry| match entry {
                ListEntry::Thread(thread)
                    if thread.metadata.session_id.as_ref() == Some(&worktree_session_id) =>
                {
                    Some(thread)
                }
                _ => None,
            })
            .expect("worktree thread should be visible in sidebar");
        assert!(
            matches!(thread.workspace, ThreadEntryWorkspace::Closed { .. }),
            "thread should start closed so archiving has to open its workspace"
        );
    });

    // Opening the missing worktree falls back to the main workspace, so the
    // thread's entry is still closed after archiving opens it.
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.archive_thread(&worktree_session_id, window, cx);
    });
    cx.run_until_parked();

    let thread_archived = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry_by_session(&worktree_session_id)
            .map(|thread| thread.archived)
    });
    assert_eq!(
        thread_archived,
        Some(true),
        "thread should be archived even though its worktree no longer exists"
    );
    assert_eq!(
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace
            .workspaces()
            .cloned()
            .collect::<Vec<_>>()),
        vec![main_workspace],
        "archiving should leave only the main workspace open"
    );
}
#[gpui::test]
async fn test_archive_selected_thread_closes_selected_agent_panel_terminal(
    cx: &mut TestAppContext,
) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    let terminal_id = panel
        .update_in(cx, |panel, window, cx| {
            panel.insert_test_terminal("Dev Server", true, window, cx)
        })
        .expect("test terminal should be inserted");
    cx.run_until_parked();

    focus_sidebar(&sidebar, cx);
    let terminal_index = sidebar.read_with(cx, |sidebar, _cx| {
        sidebar
            .contents
            .entries
            .iter()
            .position(|entry| matches!(entry, ListEntry::Terminal(terminal) if terminal.metadata.terminal_id == terminal_id))
            .expect("terminal should be visible in sidebar")
    });
    sidebar.update_in(cx, |sidebar, _window, _cx| {
        sidebar.selection = Some(terminal_index);
    });
    cx.dispatch_action(ArchiveSelectedThread);
    cx.run_until_parked();

    panel.read_with(cx, |panel, _cx| {
        assert!(!panel.has_terminal(terminal_id));
    });
    sidebar.read_with(cx, |sidebar, _cx| {
        assert!(sidebar.contents.entries.iter().all(|entry| {
            !matches!(entry, ListEntry::Terminal(terminal) if terminal.metadata.terminal_id == terminal_id)
        }));
    });
    sidebar.read_with(cx, |_sidebar, cx| {
        let store = TerminalThreadMetadataStore::global(cx).read(cx);
        assert!(
            store.entry(terminal_id).is_none(),
            "terminal metadata should be deleted when closing from the sidebar"
        );
    });
}

#[gpui::test]
async fn test_closing_active_agent_panel_terminal_activates_neighbor(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    let build_terminal_id = panel
        .update_in(cx, |panel, window, cx| {
            panel.insert_test_terminal("Build", true, window, cx)
        })
        .expect("build test terminal should be inserted");
    let server_terminal_id = panel
        .update_in(cx, |panel, window, cx| {
            panel.insert_test_terminal("Server", true, window, cx)
        })
        .expect("server test terminal should be inserted");
    cx.run_until_parked();

    let (server_metadata, server_workspace) = sidebar.read_with(cx, |sidebar, _cx| {
        sidebar
            .contents
            .entries
            .iter()
            .find_map(|entry| match entry {
                ListEntry::Terminal(terminal)
                    if terminal.metadata.terminal_id == server_terminal_id =>
                {
                    Some((terminal.metadata.clone(), terminal.workspace.clone()))
                }
                _ => None,
            })
            .expect("server terminal should be visible in sidebar")
    });
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.close_terminal(&server_metadata, &server_workspace, window, cx);
    });
    cx.run_until_parked();

    panel.read_with(cx, |panel, _cx| {
        assert!(!panel.has_terminal(server_terminal_id));
        assert_eq!(panel.active_terminal_id(), Some(build_terminal_id));
    });
    sidebar.read_with(cx, |sidebar, _cx| {
        assert!(
            matches!(&sidebar.active_entry, Some(ActiveEntry::Terminal { terminal_id, .. }) if *terminal_id == build_terminal_id),
            "expected remaining terminal to become active, got {:?}",
            sidebar.active_entry,
        );
    });
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["v [my-project]", "  Build"]
    );
}

#[gpui::test]
async fn test_parallel_threads_shown_with_live_status(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    // Open thread A and keep it generating.
    let connection = StubAgentConnection::new();
    open_thread_with_connection(&panel, connection.clone(), cx);
    send_message(&panel, cx);

    let session_id_a = active_session_id(&panel, cx);
    save_test_thread_metadata(&session_id_a, &project, cx).await;

    cx.update(|_, cx| {
        connection.send_update(
            session_id_a.clone(),
            acp_v1::SessionUpdate::AgentMessageChunk(acp_v1::ContentChunk::new(
                "working...".into(),
            )),
            cx,
        );
    });
    cx.run_until_parked();

    // Open thread B (idle, default response) — thread A goes to background.
    connection.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
        acp_v1::ContentChunk::new("Done".into()),
    )]);
    open_thread_with_connection(&panel, connection, cx);
    send_message(&panel, cx);

    let session_id_b = active_session_id(&panel, cx);
    save_test_thread_metadata(&session_id_b, &project, cx).await;

    cx.run_until_parked();

    let mut entries = visible_entries_as_strings(&sidebar, cx);
    entries[1..].sort();
    assert_eq!(
        entries,
        vec![
            //
            "v [my-project]",
            "  Hello *",
            "  Hello * (running)",
        ]
    );
}

#[gpui::test]
async fn test_subagent_permission_request_marks_parent_sidebar_thread_waiting(
    cx: &mut TestAppContext,
) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    let connection = StubAgentConnection::new().with_supports_load_session(true);
    connection.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
        acp_v1::ContentChunk::new("Done".into()),
    )]);
    open_thread_with_connection(&panel, connection, cx);
    send_message(&panel, cx);

    let parent_session_id = active_session_id(&panel, cx);
    save_test_thread_metadata(&parent_session_id, &project, cx).await;

    let subagent_session_id = acp::SessionId::new("subagent-session");
    let parent_thread = cx.update(|_, cx| panel.read(cx).active_agent_thread(cx).unwrap());
    let subagent = cx
        .update(|_, cx| {
            let parent = parent_thread.read(cx);
            parent.connection().clone().load_session(
                subagent_session_id.clone(),
                parent.project().clone(),
                PathList::default(),
                None,
                cx,
            )
        })
        .await
        .expect("subagent session");
    parent_thread.update(cx, |thread: &mut AcpThread, cx| {
        thread.register_subagent(subagent, cx);
    });
    cx.run_until_parked();

    let subagent_thread = panel.read_with(cx, |panel, cx| {
        panel
            .active_conversation_view()
            .and_then(|conversation| conversation.read(cx).thread_view(&subagent_session_id))
            .map(|thread_view| thread_view.read(cx).thread.clone())
            .expect("Expected subagent thread to be loaded into the conversation")
    });
    request_test_tool_authorization(&subagent_thread, "subagent-tool-call", "allow-subagent", cx);

    let parent_status = sidebar.read_with(cx, |sidebar, _cx| {
        sidebar
            .contents
            .entries
            .iter()
            .find_map(|entry| match entry {
                ListEntry::Thread(thread)
                    if thread.metadata.session_id.as_ref() == Some(&parent_session_id) =>
                {
                    Some(thread.status)
                }
                _ => None,
            })
            .expect("Expected parent thread entry in sidebar")
    });

    assert_eq!(parent_status, AgentThreadStatus::WaitingForConfirmation);
}

#[gpui::test]
async fn test_background_thread_completion_triggers_notification(cx: &mut TestAppContext) {
    let project_a = init_test_project_with_agent_panel("/project-a", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));
    let (sidebar, panel_a) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    // Open thread on workspace A and keep it generating.
    let connection_a = StubAgentConnection::new();
    open_thread_with_connection(&panel_a, connection_a.clone(), cx);
    send_message(&panel_a, cx);

    let session_id_a = active_session_id(&panel_a, cx);
    save_test_thread_metadata(&session_id_a, &project_a, cx).await;

    cx.update(|_, cx| {
        connection_a.send_update(
            session_id_a.clone(),
            acp_v1::SessionUpdate::AgentMessageChunk(acp_v1::ContentChunk::new("chunk".into())),
            cx,
        );
    });
    cx.run_until_parked();

    // Add a second workspace and activate it (making workspace A the background).
    let fs = cx.update(|_, cx| <dyn fs::Fs>::global(cx));
    let project_b = project::Project::test(fs, [], cx).await;
    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(project_b, window, cx);
    });
    cx.run_until_parked();

    // Thread A is still running; no notification yet.
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [project-a]",
            "  Hello * (running)",
        ]
    );

    // Complete thread A's turn (transition Running → Completed).
    connection_a.end_turn(session_id_a.clone(), acp_v1::StopReason::EndTurn);
    cx.run_until_parked();

    // The completed background thread shows a notification indicator.
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [project-a]",
            "  Hello * (!)",
        ]
    );
}

fn type_in_search(sidebar: &Entity<Sidebar>, query: &str, cx: &mut gpui::VisualTestContext) {
    sidebar.update_in(cx, |sidebar, window, cx| {
        window.focus(&sidebar.filter_editor.focus_handle(cx), cx);
        sidebar.filter_editor.update(cx, |editor, cx| {
            editor.set_text(query, window, cx);
        });
    });
    cx.run_until_parked();
}

#[gpui::test]
async fn test_search_narrows_visible_threads_to_matches(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    for (id, title, hour) in [
        ("t-1", "Fix crash in project panel", 3),
        ("t-2", "Add inline diff view", 2),
        ("t-3", "Refactor settings module", 1),
    ] {
        save_thread_metadata(
            acp::SessionId::new(Arc::from(id)),
            Some(title.into()),
            chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, hour, 0, 0).unwrap(),
            None,
            None,
            &project,
            cx,
        );
    }
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [my-project]",
            "  Fix crash in project panel",
            "  Add inline diff view",
            "  Refactor settings module",
        ]
    );

    // User types "diff" in the search box — only the matching thread remains,
    // with its workspace header preserved for context.
    type_in_search(&sidebar, "diff", cx);
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [my-project]",
            "  Add inline diff view  <== selected",
        ]
    );

    // User changes query to something with no matches — list is empty.
    type_in_search(&sidebar, "nonexistent", cx);
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        Vec::<String>::new()
    );
}

#[gpui::test]
async fn test_search_matches_regardless_of_case(cx: &mut TestAppContext) {
    // Scenario: A user remembers a thread title but not the exact casing.
    // Search should match case-insensitively so they can still find it.
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_thread_metadata(
        acp::SessionId::new(Arc::from("thread-1")),
        Some("Fix Crash In Project Panel".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        None,
        None,
        &project,
        cx,
    );
    cx.run_until_parked();

    // Lowercase query matches mixed-case title.
    type_in_search(&sidebar, "fix crash", cx);
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [my-project]",
            "  Fix Crash In Project Panel  <== selected",
        ]
    );

    // Uppercase query also matches the same title.
    type_in_search(&sidebar, "FIX CRASH", cx);
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [my-project]",
            "  Fix Crash In Project Panel  <== selected",
        ]
    );
}

#[gpui::test]
async fn test_escape_from_search_focuses_first_thread(cx: &mut TestAppContext) {
    // Scenario: A user searches, finds what they need, then presses Escape
    // in the search field to hand keyboard control back to the thread list.
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    for (id, title, hour) in [("t-1", "Alpha thread", 2), ("t-2", "Beta thread", 1)] {
        save_thread_metadata(
            acp::SessionId::new(Arc::from(id)),
            Some(title.into()),
            chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, hour, 0, 0).unwrap(),
            None,
            None,
            &project,
            cx,
        )
    }
    cx.run_until_parked();

    // Confirm the full list is showing.
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [my-project]",
            "  Alpha thread",
            "  Beta thread",
        ]
    );

    // User types a search query to filter down.
    focus_sidebar(&sidebar, cx);
    type_in_search(&sidebar, "alpha", cx);
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [my-project]",
            "  Alpha thread  <== selected",
        ]
    );

    // First Escape clears the search text, restoring the full list.
    // Focus stays on the filter editor.
    cx.dispatch_action(Cancel);
    cx.run_until_parked();
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [my-project]",
            "  Alpha thread",
            "  Beta thread",
        ]
    );
    sidebar.update_in(cx, |sidebar, window, cx| {
        assert!(sidebar.filter_editor.read(cx).is_focused(window));
        assert!(!sidebar.focus_handle.is_focused(window));
    });

    // Second Escape moves focus from the empty search field to the thread list.
    cx.dispatch_action(Cancel);
    cx.run_until_parked();
    sidebar.update_in(cx, |sidebar, window, cx| {
        assert_eq!(sidebar.selection, Some(1));
        assert!(sidebar.focus_handle.is_focused(window));
        assert!(!sidebar.filter_editor.read(cx).is_focused(window));
    });
}

#[gpui::test]
async fn test_search_only_shows_workspace_headers_with_matches(cx: &mut TestAppContext) {
    let project_a = init_test_project("/project-a", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    for (id, title, hour) in [
        ("a1", "Fix bug in sidebar", 2),
        ("a2", "Add tests for editor", 1),
    ] {
        save_thread_metadata(
            acp::SessionId::new(Arc::from(id)),
            Some(title.into()),
            chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, hour, 0, 0).unwrap(),
            None,
            None,
            &project_a,
            cx,
        )
    }

    // Add a second workspace.
    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.create_test_workspace(window, cx).detach();
    });
    cx.run_until_parked();

    let project_b = multi_workspace.read_with(cx, |mw, cx| {
        mw.workspaces().nth(1).unwrap().read(cx).project().clone()
    });

    for (id, title, hour) in [
        ("b1", "Refactor sidebar layout", 3),
        ("b2", "Fix typo in README", 1),
    ] {
        save_thread_metadata(
            acp::SessionId::new(Arc::from(id)),
            Some(title.into()),
            chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, hour, 0, 0).unwrap(),
            None,
            None,
            &project_b,
            cx,
        )
    }
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [project-a]",
            "  Fix bug in sidebar",
            "  Add tests for editor",
        ]
    );

    // "sidebar" matches a thread in each workspace — both headers stay visible.
    type_in_search(&sidebar, "sidebar", cx);
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [project-a]",
            "  Fix bug in sidebar  <== selected",
        ]
    );

    // "typo" only matches in the second workspace — the first header disappears.
    type_in_search(&sidebar, "typo", cx);
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        Vec::<String>::new()
    );

    // "project-a" matches the first workspace name — the header appears
    // with all child threads included.
    type_in_search(&sidebar, "project-a", cx);
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [project-a]",
            "  Fix bug in sidebar  <== selected",
            "  Add tests for editor",
        ]
    );
}

#[gpui::test]
async fn test_search_matches_workspace_name(cx: &mut TestAppContext) {
    let project_a = init_test_project("/alpha-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    for (id, title, hour) in [
        ("a1", "Fix bug in sidebar", 2),
        ("a2", "Add tests for editor", 1),
    ] {
        save_thread_metadata(
            acp::SessionId::new(Arc::from(id)),
            Some(title.into()),
            chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, hour, 0, 0).unwrap(),
            None,
            None,
            &project_a,
            cx,
        )
    }

    // Add a second workspace.
    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.create_test_workspace(window, cx).detach();
    });
    cx.run_until_parked();

    let project_b = multi_workspace.read_with(cx, |mw, cx| {
        mw.workspaces().nth(1).unwrap().read(cx).project().clone()
    });

    for (id, title, hour) in [
        ("b1", "Refactor sidebar layout", 3),
        ("b2", "Fix typo in README", 1),
    ] {
        save_thread_metadata(
            acp::SessionId::new(Arc::from(id)),
            Some(title.into()),
            chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, hour, 0, 0).unwrap(),
            None,
            None,
            &project_b,
            cx,
        )
    }
    cx.run_until_parked();

    // "alpha" matches the workspace name "alpha-project" but no thread titles.
    // The workspace header should appear with all child threads included.
    type_in_search(&sidebar, "alpha", cx);
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [alpha-project]",
            "  Fix bug in sidebar  <== selected",
            "  Add tests for editor",
        ]
    );

    // "sidebar" matches thread titles in both workspaces but not workspace names.
    // Both headers appear with their matching threads.
    type_in_search(&sidebar, "sidebar", cx);
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [alpha-project]",
            "  Fix bug in sidebar  <== selected",
        ]
    );

    // "alpha sidebar" matches the workspace name "alpha-project" (fuzzy: a-l-p-h-a-s-i-d-e-b-a-r
    // doesn't match) — but does not match either workspace name or any thread.
    // Actually let's test something simpler: a query that matches both a workspace
    // name AND some threads in that workspace. Matching threads should still appear.
    type_in_search(&sidebar, "fix", cx);
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [alpha-project]",
            "  Fix bug in sidebar  <== selected",
        ]
    );

    // A query that matches a workspace name AND a thread in that same workspace.
    // Both the header (highlighted) and all child threads should appear.
    type_in_search(&sidebar, "alpha", cx);
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [alpha-project]",
            "  Fix bug in sidebar  <== selected",
            "  Add tests for editor",
        ]
    );

    // Now search for something that matches only a workspace name when there
    // are also threads with matching titles — the non-matching workspace's
    // threads should still appear if their titles match.
    type_in_search(&sidebar, "alp", cx);
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [alpha-project]",
            "  Fix bug in sidebar  <== selected",
            "  Add tests for editor",
        ]
    );
}

#[gpui::test]
async fn test_search_finds_threads_inside_collapsed_groups(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_thread_metadata(
        acp::SessionId::new(Arc::from("thread-1")),
        Some("Important thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        None,
        None,
        &project,
        cx,
    );
    cx.run_until_parked();

    // User focuses the sidebar and collapses the group using keyboard:
    // manually select the header, then press SelectParent to collapse.
    focus_sidebar(&sidebar, cx);
    sidebar.update_in(cx, |sidebar, _window, _cx| {
        sidebar.selection = Some(0);
    });
    cx.dispatch_action(SelectParent);
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "> [my-project]  <== selected",
        ]
    );

    // User types a search — the thread appears even though its group is collapsed.
    type_in_search(&sidebar, "important", cx);
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "> [my-project]",
            "  Important thread  <== selected",
        ]
    );
}

#[gpui::test]
async fn test_search_then_keyboard_navigate_and_confirm(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    for (id, title, hour) in [
        ("t-1", "Fix crash in panel", 3),
        ("t-2", "Fix lint warnings", 2),
        ("t-3", "Add new feature", 1),
    ] {
        save_thread_metadata(
            acp::SessionId::new(Arc::from(id)),
            Some(title.into()),
            chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, hour, 0, 0).unwrap(),
            None,
            None,
            &project,
            cx,
        )
    }
    cx.run_until_parked();

    focus_sidebar(&sidebar, cx);

    // User types "fix" — two threads match.
    type_in_search(&sidebar, "fix", cx);
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [my-project]",
            "  Fix crash in panel  <== selected",
            "  Fix lint warnings",
        ]
    );

    // Selection starts on the first matching thread. User presses
    // SelectNext to move to the second match.
    cx.dispatch_action(SelectNext);
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [my-project]",
            "  Fix crash in panel",
            "  Fix lint warnings  <== selected",
        ]
    );

    // User can also jump back with SelectPrevious.
    cx.dispatch_action(SelectPrevious);
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [my-project]",
            "  Fix crash in panel  <== selected",
            "  Fix lint warnings",
        ]
    );
}

#[gpui::test]
async fn test_confirm_on_historical_thread_activates_workspace(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.create_test_workspace(window, cx).detach();
    });
    cx.run_until_parked();

    let (workspace_0, workspace_1) = multi_workspace.read_with(cx, |mw, _| {
        (
            mw.workspaces().next().unwrap().clone(),
            mw.workspaces().nth(1).unwrap().clone(),
        )
    });

    save_thread_metadata(
        acp::SessionId::new(Arc::from("hist-1")),
        Some("Historical Thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 6, 1, 0, 0, 0).unwrap(),
        None,
        None,
        &project,
        cx,
    );
    cx.run_until_parked();
    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [my-project]",
            "  Historical Thread",
        ]
    );

    // Switch to workspace 1 so we can verify the confirm switches back.
    multi_workspace.update_in(cx, |mw, window, cx| {
        let workspace = mw.workspaces().nth(1).unwrap().clone();
        mw.activate(workspace, None, window, cx);
    });
    cx.run_until_parked();
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspace().clone()),
        workspace_1
    );

    // Confirm on the historical (non-live) thread at index 1.
    // Before a previous fix, the workspace field was Option<usize> and
    // historical threads had None, so activate_thread early-returned
    // without switching the workspace.
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.selection = Some(1);
        sidebar.confirm(&Confirm, window, cx);
    });
    cx.run_until_parked();

    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspace().clone()),
        workspace_0
    );
}

#[gpui::test]
async fn test_confirm_on_historical_thread_preserves_historical_timestamp_and_order(
    cx: &mut TestAppContext,
) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, _panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    let newer_session_id = acp::SessionId::new(Arc::from("newer-historical-thread"));
    let newer_timestamp = chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 6, 2, 0, 0, 0).unwrap();
    save_thread_metadata(
        newer_session_id,
        Some("Newer Historical Thread".into()),
        newer_timestamp,
        Some(newer_timestamp),
        None,
        &project,
        cx,
    );

    let older_session_id = acp::SessionId::new(Arc::from("older-historical-thread"));
    let older_timestamp = chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 6, 1, 0, 0, 0).unwrap();
    save_thread_metadata(
        older_session_id.clone(),
        Some("Older Historical Thread".into()),
        older_timestamp,
        Some(older_timestamp),
        None,
        &project,
        cx,
    );

    cx.run_until_parked();
    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    let historical_entries_before: Vec<_> = visible_entries_as_strings(&sidebar, cx)
        .into_iter()
        .filter(|entry| entry.contains("Historical Thread"))
        .collect();
    assert_eq!(
        historical_entries_before,
        vec![
            "  Newer Historical Thread".to_string(),
            "  Older Historical Thread".to_string(),
        ],
        "expected the sidebar to sort historical threads by their saved timestamp before activation"
    );

    let older_entry_index = sidebar.read_with(cx, |sidebar, _cx| {
        sidebar
            .contents
            .entries
            .iter()
            .position(|entry| {
                matches!(entry, ListEntry::Thread(thread)
                    if thread.metadata.session_id.as_ref() == Some(&older_session_id))
            })
            .expect("expected Older Historical Thread to appear in the sidebar")
    });

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.selection = Some(older_entry_index);
        sidebar.confirm(&Confirm, window, cx);
    });
    cx.run_until_parked();

    let older_metadata = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry_by_session(&older_session_id)
            .cloned()
            .expect("expected metadata for Older Historical Thread after activation")
    });
    assert_eq!(
        older_metadata.created_at,
        Some(older_timestamp),
        "activating a historical thread should not rewrite its saved created_at timestamp"
    );

    let historical_entries_after: Vec<_> = visible_entries_as_strings(&sidebar, cx)
        .into_iter()
        .filter(|entry| entry.contains("Historical Thread"))
        .collect();
    assert_eq!(
        historical_entries_after,
        vec![
            "  Newer Historical Thread".to_string(),
            "  Older Historical Thread  <== selected".to_string(),
        ],
        "activating an older historical thread should not reorder it ahead of a newer historical thread"
    );
}

#[gpui::test]
async fn test_confirm_on_historical_thread_in_new_project_group_opens_real_thread(
    cx: &mut TestAppContext,
) {
    use workspace::ProjectGroup;

    agent_ui::test_support::init_test(cx);
    cx.update(|cx| {
        set_max_idle_retained_threads(1, cx);
        ThreadStore::init_global(cx);
        ThreadMetadataStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
        prompt_store::init(cx);
    });

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/project-a", serde_json::json!({ "src": {} }))
        .await;
    fs.insert_tree("/project-b", serde_json::json!({ "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let project_a = project::Project::test(fs.clone(), ["/project-a".as_ref()], cx).await;
    let project_b = project::Project::test(fs.clone(), ["/project-b".as_ref()], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    let project_b_key = project_b.read_with(cx, |project, cx| project.project_group_key(cx));
    multi_workspace.update(cx, |mw, _cx| {
        mw.test_add_project_group(ProjectGroup {
            key: project_b_key.clone(),
            workspaces: Vec::new(),
            expanded: true,
        });
    });

    let session_id = acp::SessionId::new(Arc::from("historical-new-project-group"));
    save_thread_metadata(
        session_id.clone(),
        Some("Historical Thread in New Group".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 6, 1, 0, 0, 0).unwrap(),
        None,
        None,
        &project_b,
        cx,
    );
    cx.run_until_parked();

    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    let entries_before = visible_entries_as_strings(&sidebar, cx);
    assert_eq!(
        entries_before,
        vec![
            "v [project-a]",
            "v [project-b]",
            "  Historical Thread in New Group",
        ],
        "expected the closed project group to show the historical thread before first open"
    );

    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        1,
        "should start without an open workspace for the new project group"
    );

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.selection = Some(2);
        sidebar.confirm(&Confirm, window, cx);
    });

    cx.run_until_parked();

    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        2,
        "confirming the historical thread should open a workspace for the new project group"
    );

    let workspace_b = multi_workspace.read_with(cx, |mw, cx| {
        mw.workspaces()
            .find(|workspace| {
                PathList::new(&workspace.read(cx).root_paths(cx))
                    == project_b_key.path_list().clone()
            })
            .cloned()
            .expect("expected workspace for project-b after opening the historical thread")
    });

    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspace().clone()),
        workspace_b,
        "opening the historical thread should activate the new project's workspace"
    );

    let panel = workspace_b.read_with(cx, |workspace, cx| {
        workspace
            .panel::<AgentPanel>(cx)
            .expect("expected first-open activation to bootstrap the agent panel")
    });

    let expected_thread_id = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entries()
            .find(|e| e.session_id.as_ref() == Some(&session_id))
            .map(|e| e.thread_id)
            .expect("metadata should still map session id to thread id")
    });

    assert_eq!(
        panel.read_with(cx, |panel, cx| panel.active_thread_id(cx)),
        Some(expected_thread_id),
        "expected the agent panel to activate the real historical thread rather than a draft"
    );

    let entries_after = visible_entries_as_strings(&sidebar, cx);
    let matching_rows: Vec<_> = entries_after
        .iter()
        .filter(|entry| entry.contains("Historical Thread in New Group") || entry.contains("Draft"))
        .cloned()
        .collect();
    assert_eq!(
        matching_rows.len(),
        1,
        "expected only one matching row after first open into a new project group, got entries: {entries_after:?}"
    );
    assert!(
        matching_rows[0].contains("Historical Thread in New Group"),
        "expected the surviving row to be the real historical thread, got entries: {entries_after:?}"
    );
    assert!(
        !matching_rows[0].contains("Draft"),
        "expected no draft row after first open into a new project group, got entries: {entries_after:?}"
    );
}

struct SidebarClickFixture {
    sidebar: Entity<Sidebar>,
    panel: Entity<AgentPanel>,
}

impl SidebarClickFixture {
    fn new(multi_workspace: &Entity<MultiWorkspace>, cx: &mut gpui::VisualTestContext) -> Self {
        let (sidebar, panel) = setup_sidebar_with_agent_panel(multi_workspace, cx);
        Self { sidebar, panel }
    }

    fn insert_terminal(&self, title: &str, cx: &mut gpui::VisualTestContext) -> TerminalId {
        let terminal_id = self
            .panel
            .update_in(cx, |panel, window, cx| {
                panel.insert_test_terminal(title, true, window, cx)
            })
            .expect("test terminal should be inserted");
        cx.run_until_parked();
        terminal_id
    }

    fn terminal_index(&self, terminal_id: TerminalId, cx: &mut gpui::VisualTestContext) -> usize {
        self.sidebar.read_with(cx, |sidebar, _cx| {
            sidebar
                .contents
                .entries
                .iter()
                .position(|entry| {
                    matches!(
                        entry,
                        ListEntry::Terminal(terminal)
                            if terminal.metadata.terminal_id == terminal_id
                    )
                })
                .expect("terminal should be visible in sidebar")
        })
    }

    fn thread_index(&self, session_id: &acp::SessionId, cx: &mut gpui::VisualTestContext) -> usize {
        self.sidebar.read_with(cx, |sidebar, _cx| {
            sidebar
                .contents
                .entries
                .iter()
                .position(|entry| {
                    matches!(
                        entry,
                        ListEntry::Thread(thread)
                            if thread.metadata.session_id.as_ref() == Some(session_id)
                    )
                })
                .expect("thread should be visible in sidebar")
        })
    }

    fn select_and_focus(&self, entry_index: usize, cx: &mut gpui::VisualTestContext) {
        self.sidebar.update_in(cx, |sidebar, window, cx| {
            sidebar.selection = Some(entry_index);
            sidebar.focus_handle.focus(window, cx);
            cx.notify();
        });
        cx.run_until_parked();
    }

    fn click(&self, entry_index: usize, cx: &mut gpui::VisualTestContext) {
        cx.draw(
            gpui::point(px(0.), px(0.)),
            gpui::size(px(400.), px(400.)),
            |_, _| self.sidebar.clone().into_any_element(),
        );
        let entry_bounds = self.sidebar.read_with(cx, |sidebar, _cx| {
            sidebar
                .list_state
                .bounds_for_item(entry_index)
                .expect("sidebar entry should be measured")
        });
        cx.simulate_click(entry_bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
    }
}

#[gpui::test]
async fn test_keyboard_confirm_on_terminal_preserves_selection(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let fixture = SidebarClickFixture::new(&multi_workspace, cx);
    let terminal_id = fixture.insert_terminal("Terminal A", cx);
    let other_terminal_id = fixture.insert_terminal("Terminal B", cx);
    let terminal_index = fixture.terminal_index(terminal_id, cx);
    fixture.select_and_focus(terminal_index, cx);

    fixture.sidebar.update_in(cx, |sidebar, window, _cx| {
        assert!(sidebar.focus_handle.is_focused(window));
        assert_eq!(sidebar.selection, Some(terminal_index));
        assert!(
            matches!(
                &sidebar.active_entry,
                Some(ActiveEntry::Terminal { terminal_id: active_terminal_id, .. })
                    if *active_terminal_id == other_terminal_id
            ),
            "Terminal B should be active before confirming Terminal A, got {:?}",
            sidebar.active_entry,
        );
    });

    // Confirm switches from Terminal B to Terminal A without clearing selection.
    cx.dispatch_action(Confirm);
    cx.run_until_parked();
    fixture.sidebar.read_with(cx, |sidebar, _cx| {
        assert_eq!(sidebar.selection, Some(terminal_index));
        assert!(
            matches!(
                &sidebar.active_entry,
                Some(ActiveEntry::Terminal { terminal_id: active_terminal_id, .. })
                    if *active_terminal_id == terminal_id
            ),
            "confirmed terminal should be active, got {:?}",
            sidebar.active_entry,
        );
    });
}

#[gpui::test]
async fn test_keyboard_confirm_on_thread_preserves_selection(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let fixture = SidebarClickFixture::new(&multi_workspace, cx);
    let session_id = acp::SessionId::new(Arc::from("thread-a"));
    save_thread_metadata(
        session_id.clone(),
        Some("Thread A".into()),
        Utc::now(),
        None,
        None,
        &project,
        cx,
    );
    let thread_index = fixture.thread_index(&session_id, cx);
    fixture.select_and_focus(thread_index, cx);

    // Confirm must preserve selection without the test setting it again.
    cx.dispatch_action(Confirm);
    cx.run_until_parked();
    fixture.sidebar.read_with(cx, |sidebar, _cx| {
        assert_eq!(sidebar.selection, Some(thread_index));
        assert_active_thread(sidebar, &session_id, "confirmed thread should be active");
    });
}

#[gpui::test]
async fn test_clicking_different_terminal_clears_sidebar_selection(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let fixture = SidebarClickFixture::new(&multi_workspace, cx);
    let terminal_a_id = fixture.insert_terminal("Terminal A", cx);
    let terminal_b_id = fixture.insert_terminal("Terminal B", cx);
    let terminal_a_index = fixture.terminal_index(terminal_a_id, cx);
    let terminal_b_index = fixture.terminal_index(terminal_b_id, cx);

    fixture.panel.update_in(cx, |panel, window, cx| {
        panel.activate_terminal(terminal_a_id, true, window, cx);
    });
    cx.run_until_parked();
    fixture.select_and_focus(terminal_a_index, cx);

    fixture.sidebar.update_in(cx, |sidebar, window, _cx| {
        assert!(sidebar.focus_handle.is_focused(window));
        assert_eq!(sidebar.selection, Some(terminal_a_index));
        assert!(
            matches!(
                &sidebar.active_entry,
                Some(ActiveEntry::Terminal { terminal_id, .. })
                    if *terminal_id == terminal_a_id
            ),
            "Terminal A should be active before clicking Terminal B, got {:?}",
            sidebar.active_entry,
        );
    });

    // Clicking a different terminal clears keyboard selection and activates that terminal.
    fixture.click(terminal_b_index, cx);

    fixture.sidebar.read_with(cx, |sidebar, _cx| {
        assert_eq!(sidebar.selection, None);
        assert!(
            matches!(
                &sidebar.active_entry,
                Some(ActiveEntry::Terminal { terminal_id, .. })
                    if *terminal_id == terminal_b_id
            ),
            "Terminal B should be active after the click, got {:?}",
            sidebar.active_entry,
        );
    });
}

#[gpui::test]
async fn test_clicking_active_terminal_clears_sidebar_selection(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let fixture = SidebarClickFixture::new(&multi_workspace, cx);
    let terminal_id = fixture.insert_terminal("Terminal A", cx);
    let terminal_index = fixture.terminal_index(terminal_id, cx);
    fixture.select_and_focus(terminal_index, cx);

    fixture.sidebar.update_in(cx, |sidebar, window, _cx| {
        assert!(sidebar.focus_handle.is_focused(window));
        assert_eq!(sidebar.selection, Some(terminal_index));
        assert!(
            matches!(
                &sidebar.active_entry,
                Some(ActiveEntry::Terminal { terminal_id: active_terminal_id, .. })
                    if *active_terminal_id == terminal_id
            ),
            "Terminal A should be active before clicking it, got {:?}",
            sidebar.active_entry,
        );
    });

    // Clicking the active terminal clears keyboard selection without changing the active terminal.
    fixture.click(terminal_index, cx);

    fixture.sidebar.read_with(cx, |sidebar, _cx| {
        assert_eq!(sidebar.selection, None);
        assert!(
            matches!(
                &sidebar.active_entry,
                Some(ActiveEntry::Terminal { terminal_id: active_terminal_id, .. })
                    if *active_terminal_id == terminal_id
            ),
            "Terminal A should remain active after the click, got {:?}",
            sidebar.active_entry,
        );
    });
}

#[gpui::test]
async fn test_clicking_different_thread_clears_sidebar_selection(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let fixture = SidebarClickFixture::new(&multi_workspace, cx);
    let thread_a_session_id = acp::SessionId::new(Arc::from("thread-a"));
    let thread_b_session_id = acp::SessionId::new(Arc::from("thread-b"));
    save_thread_metadata(
        thread_a_session_id.clone(),
        Some("Thread A".into()),
        Utc::now() + chrono::Duration::days(2),
        None,
        None,
        &project,
        cx,
    );
    save_thread_metadata(
        thread_b_session_id.clone(),
        Some("Thread B".into()),
        Utc::now() + chrono::Duration::days(1),
        None,
        None,
        &project,
        cx,
    );
    let thread_a_index = fixture.thread_index(&thread_a_session_id, cx);
    let thread_b_index = fixture.thread_index(&thread_b_session_id, cx);
    fixture.select_and_focus(thread_a_index, cx);
    cx.dispatch_action(Confirm);
    cx.run_until_parked();
    fixture.select_and_focus(thread_a_index, cx);

    fixture.sidebar.update_in(cx, |sidebar, window, _cx| {
        assert!(sidebar.focus_handle.is_focused(window));
        assert_eq!(sidebar.selection, Some(thread_a_index));
        assert_active_thread(
            sidebar,
            &thread_a_session_id,
            "Thread A should be active before clicking Thread B",
        );
    });

    // Clicking a different thread clears keyboard selection and activates that thread.
    fixture.click(thread_b_index, cx);

    fixture.sidebar.read_with(cx, |sidebar, _cx| {
        assert_eq!(sidebar.selection, None);
        assert_active_thread(
            sidebar,
            &thread_b_session_id,
            "Thread B should be active after the click",
        );
    });
}

#[gpui::test]
async fn test_thread_title_update_propagates_to_sidebar(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    let connection = StubAgentConnection::new();
    connection.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
        acp_v1::ContentChunk::new("Hi there!".into()),
    )]);
    open_thread_with_connection(&panel, connection, cx);
    send_message(&panel, cx);

    let session_id = active_session_id(&panel, cx);
    save_test_thread_metadata(&session_id, &project, cx).await;
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [my-project]",
            "  Hello *",
        ]
    );

    // Simulate the agent generating a title. The notification chain is:
    // AcpThread::set_title emits TitleUpdated →
    // ConnectionView::handle_thread_event calls cx.notify() →
    // AgentPanel observer fires and emits AgentPanelEvent →
    // Sidebar subscription calls update_entries / rebuild_contents.
    //
    // Before the fix, handle_thread_event did NOT call cx.notify() for
    // TitleUpdated, so the AgentPanel observer never fired and the
    // sidebar kept showing the old title.
    let thread = panel.read_with(cx, |panel, cx| panel.active_agent_thread(cx).unwrap());
    thread.update(cx, |thread, cx| {
        thread
            .set_title("Friendly Greeting with AI".into(), cx)
            .detach();
    });
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [my-project]",
            "  Friendly Greeting with AI *",
        ]
    );
}

#[gpui::test]
async fn test_rename_thread_from_sidebar_updates_title_override(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    let connection = StubAgentConnection::new();
    connection.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
        acp_v1::ContentChunk::new("Hi there!".into()),
    )]);
    open_thread_with_connection(&panel, connection, cx);
    send_message(&panel, cx);

    let session_id = active_session_id(&panel, cx);
    save_test_thread_metadata(&session_id, &project, cx).await;
    cx.run_until_parked();

    let (entry_ix, thread_id, title) = sidebar.read_with(cx, |sidebar, _cx| {
        sidebar
            .contents
            .entries
            .iter()
            .enumerate()
            .find_map(|(ix, entry)| match entry {
                ListEntry::Thread(thread) => Some((
                    ix,
                    thread.metadata.thread_id,
                    thread.metadata.display_title(),
                )),
                ListEntry::ProjectHeader { .. } | ListEntry::Terminal(_) => None,
            })
            .expect("sidebar should have a thread entry")
    });

    let renamed_title = "abcdefghijklmnopqrstuvwxyé renamed";
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.start_renaming_entry(entry_ix, RenameTarget::Thread(thread_id), title, window, cx);
    });
    cx.run_until_parked();
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.rename_editor.update(cx, |editor, cx| {
            editor.set_text(renamed_title, window, cx);
        });
    });
    cx.run_until_parked();
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.finish_entry_rename(window, cx);
    });
    cx.run_until_parked();

    let metadata = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry(thread_id)
            .cloned()
            .expect("thread metadata should exist")
    });
    assert_eq!(metadata.title_override.as_deref(), Some(renamed_title));
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [my-project]",
            "  abcdefghijklmnopqrstuvwxyé renamed *  <== selected",
        ]
    );

    let active_thread = panel.read_with(cx, |panel, cx| panel.active_agent_thread(cx).unwrap());
    assert_eq!(
        active_thread.read_with(cx, |thread, _| thread.title()),
        Some(renamed_title.into())
    );
    let active_thread_view = panel.read_with(cx, |panel, cx| panel.active_thread_view(cx).unwrap());
    let title_editor_text =
        active_thread_view.read_with(cx, |view, cx| view.title_editor.read(cx).text(cx));
    assert_eq!(title_editor_text, renamed_title);

    active_thread.update(cx, |thread, cx| {
        thread
            .set_title("abcdefghijklmnopqrstuvwxyz0".into(), cx)
            .detach();
    });
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [my-project]",
            "  abcdefghijklmnopqrstuvwxyé renamed *  <== selected",
        ]
    );

    type_in_search(&sidebar, "0", cx);
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        Vec::<String>::new()
    );

    type_in_search(&sidebar, "é", cx);
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [my-project]",
            "  abcdefghijklmnopqrstuvwxyé renamed *  <== selected",
        ]
    );
    sidebar.read_with(cx, |sidebar, _cx| {
        let thread = sidebar
            .contents
            .entries
            .iter()
            .find_map(|entry| match entry {
                ListEntry::Thread(thread) => Some(thread),
                ListEntry::ProjectHeader { .. } | ListEntry::Terminal(_) => None,
            })
            .expect("renamed thread should match the search");
        let title = thread.metadata.display_title();
        assert!(
            thread
                .highlight_positions
                .iter()
                .all(|position| { title.is_char_boundary(*position) })
        );
    });
}

#[gpui::test]
async fn test_rename_selected_thread_action_renames_selected_thread(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    let connection = StubAgentConnection::new();
    connection.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
        acp_v1::ContentChunk::new("Hi there!".into()),
    )]);
    open_thread_with_connection(&panel, connection, cx);
    send_message(&panel, cx);

    let session_id = active_session_id(&panel, cx);
    save_test_thread_metadata(&session_id, &project, cx).await;
    cx.run_until_parked();

    let (entry_ix, thread_id) = sidebar.read_with(cx, |sidebar, _cx| {
        sidebar
            .contents
            .entries
            .iter()
            .enumerate()
            .find_map(|(ix, entry)| match entry {
                ListEntry::Thread(thread) => Some((ix, thread.metadata.thread_id)),
                ListEntry::ProjectHeader { .. } | ListEntry::Terminal(_) => None,
            })
            .expect("sidebar should have a thread entry")
    });

    focus_sidebar(&sidebar, cx);
    sidebar.update_in(cx, |sidebar, _window, _cx| {
        sidebar.selection = Some(entry_ix);
    });
    cx.dispatch_action(RenameSelectedThread);
    cx.run_until_parked();

    let renamed_title = "Renamed via action";
    enter_renamed_title(&sidebar, RenameTarget::Thread(thread_id), renamed_title, cx);

    let metadata = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry(thread_id)
            .cloned()
            .expect("thread metadata should exist")
    });
    assert_eq!(metadata.title_override.as_deref(), Some(renamed_title));
}

#[gpui::test]
async fn test_rename_selected_thread_action_renames_terminal(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    let terminal_id = panel
        .update_in(cx, |panel, window, cx| {
            panel.insert_test_terminal("Dev Server", true, window, cx)
        })
        .expect("test terminal should be inserted");
    cx.run_until_parked();

    let entry_ix = sidebar.read_with(cx, |sidebar, _cx| {
        sidebar
            .contents
            .entries
            .iter()
            .position(|entry| {
                matches!(
                    entry,
                    ListEntry::Terminal(terminal)
                        if terminal.metadata.terminal_id == terminal_id
                )
            })
            .expect("sidebar should have a terminal entry")
    });

    focus_sidebar(&sidebar, cx);
    sidebar.update_in(cx, |sidebar, _window, _cx| {
        sidebar.selection = Some(entry_ix);
    });
    cx.dispatch_action(RenameSelectedThread);
    cx.run_until_parked();

    let renamed_title = "Renamed Terminal";
    enter_renamed_title(
        &sidebar,
        RenameTarget::Terminal(terminal_id),
        renamed_title,
        cx,
    );

    panel.read_with(cx, |panel, cx| {
        let terminal = panel
            .terminals(cx)
            .into_iter()
            .find(|terminal| terminal.id == terminal_id)
            .expect("terminal should remain open after renaming");
        assert_eq!(terminal.custom_title.as_deref(), Some(renamed_title));
    });
    sidebar.read_with(cx, |_sidebar, cx| {
        let metadata = TerminalThreadMetadataStore::global(cx)
            .read(cx)
            .entry(terminal_id)
            .cloned()
            .expect("renamed terminal metadata should exist");
        assert_eq!(metadata.custom_title.as_deref(), Some(renamed_title));
    });
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["v [my-project]", "  Renamed Terminal  <== selected"]
    );
}

#[gpui::test]
async fn test_focused_thread_tracks_user_intent(cx: &mut TestAppContext) {
    let project_a = init_test_project_with_agent_panel("/project-a", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));
    let (sidebar, panel_a) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    // Save a thread so it appears in the list.
    let connection_a = StubAgentConnection::new();
    connection_a.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
        acp_v1::ContentChunk::new("Done".into()),
    )]);
    open_thread_with_connection(&panel_a, connection_a, cx);
    send_message(&panel_a, cx);
    let session_id_a = active_session_id(&panel_a, cx);
    save_test_thread_metadata(&session_id_a, &project_a, cx).await;

    // Add a second workspace with its own agent panel.
    let fs = cx.update(|_, cx| <dyn fs::Fs>::global(cx));
    fs.as_fake()
        .insert_tree("/project-b", serde_json::json!({ "src": {} }))
        .await;
    let project_b = project::Project::test(fs, ["/project-b".as_ref()], cx).await;
    let workspace_b = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(project_b.clone(), window, cx)
    });
    let panel_b = add_agent_panel(&workspace_b, cx);
    cx.run_until_parked();

    let workspace_a =
        multi_workspace.read_with(cx, |mw, _cx| mw.workspaces().next().unwrap().clone());

    // ── 1. Initial state: focused thread derived from active panel ─────
    sidebar.read_with(cx, |sidebar, _cx| {
        assert_active_thread(
            sidebar,
            &session_id_a,
            "The active panel's thread should be focused on startup",
        );
    });

    let thread_metadata_a = cx.update(|_window, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry_by_session(&session_id_a)
            .cloned()
            .expect("session_id_a should exist in metadata store")
    });
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.activate_thread(thread_metadata_a, &workspace_a, false, window, cx);
    });
    cx.run_until_parked();

    sidebar.read_with(cx, |sidebar, _cx| {
        assert_active_thread(
            sidebar,
            &session_id_a,
            "After clicking a thread, it should be the focused thread",
        );
        assert!(
            has_thread_entry(sidebar, &session_id_a),
            "The clicked thread should be present in the entries"
        );
    });

    workspace_a.read_with(cx, |workspace, cx| {
        assert!(
            workspace.panel::<AgentPanel>(cx).is_some(),
            "Agent panel should exist"
        );
        let dock = workspace.left_dock().read(cx);
        assert!(
            dock.is_open(),
            "Clicking a thread should open the agent panel dock"
        );
    });

    let connection_b = StubAgentConnection::new();
    connection_b.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
        acp_v1::ContentChunk::new("Thread B".into()),
    )]);
    open_thread_with_connection(&panel_b, connection_b, cx);
    send_message(&panel_b, cx);
    let session_id_b = active_session_id(&panel_b, cx);
    save_test_thread_metadata(&session_id_b, &project_b, cx).await;
    cx.run_until_parked();

    // Workspace A is currently active. Click a thread in workspace B,
    // which also triggers a workspace switch.
    let thread_metadata_b = cx.update(|_window, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry_by_session(&session_id_b)
            .cloned()
            .expect("session_id_b should exist in metadata store")
    });
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.activate_thread(thread_metadata_b, &workspace_b, false, window, cx);
    });
    cx.run_until_parked();

    sidebar.read_with(cx, |sidebar, _cx| {
        assert_active_thread(
            sidebar,
            &session_id_b,
            "Clicking a thread in another workspace should focus that thread",
        );
        assert!(
            has_thread_entry(sidebar, &session_id_b),
            "The cross-workspace thread should be present in the entries"
        );
    });

    multi_workspace.update_in(cx, |mw, window, cx| {
        let workspace = mw.workspaces().next().unwrap().clone();
        mw.activate(workspace, None, window, cx);
    });
    cx.run_until_parked();

    sidebar.read_with(cx, |sidebar, _cx| {
        assert_active_thread(
            sidebar,
            &session_id_a,
            "Switching workspace should seed focused_thread from the new active panel",
        );
        assert!(
            has_thread_entry(sidebar, &session_id_a),
            "The seeded thread should be present in the entries"
        );
    });

    let connection_b2 = StubAgentConnection::new();
    connection_b2.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
        acp_v1::ContentChunk::new(DEFAULT_THREAD_TITLE.into()),
    )]);
    open_thread_with_connection(&panel_b, connection_b2, cx);
    send_message(&panel_b, cx);
    let session_id_b2 = active_session_id(&panel_b, cx);
    save_test_thread_metadata(&session_id_b2, &project_b, cx).await;
    cx.run_until_parked();

    // Panel B is not the active workspace's panel (workspace A is
    // active), so opening a thread there should not change focused_thread.
    // This prevents running threads in background workspaces from causing
    // the selection highlight to jump around.
    sidebar.read_with(cx, |sidebar, _cx| {
        assert_active_thread(
            sidebar,
            &session_id_a,
            "Opening a thread in a non-active panel should not change focused_thread",
        );
    });

    workspace_b.update_in(cx, |workspace, window, cx| {
        workspace.focus_handle(cx).focus(window, cx);
    });
    cx.run_until_parked();

    sidebar.read_with(cx, |sidebar, _cx| {
        assert_active_thread(
            sidebar,
            &session_id_a,
            "Defocusing the sidebar should not change focused_thread",
        );
    });

    // Switching workspaces via the multi_workspace (simulates clicking
    // a workspace header) should clear focused_thread.
    multi_workspace.update_in(cx, |mw, window, cx| {
        let workspace = mw.workspaces().find(|w| *w == &workspace_b).cloned();
        if let Some(workspace) = workspace {
            mw.activate(workspace, None, window, cx);
        }
    });
    cx.run_until_parked();

    sidebar.read_with(cx, |sidebar, _cx| {
        assert_active_thread(
            sidebar,
            &session_id_b2,
            "Switching workspace should seed focused_thread from the new active panel",
        );
        assert!(
            has_thread_entry(sidebar, &session_id_b2),
            "The seeded thread should be present in the entries"
        );
    });

    // ── 8. Focusing the agent panel thread keeps focused_thread ────
    // Workspace B still has session_id_b2 loaded in the agent panel.
    // Clicking into the thread (simulated by focusing its view) should
    // keep focused_thread since it was already seeded on workspace switch.
    panel_b.update_in(cx, |panel, window, cx| {
        if let Some(thread_view) = panel.active_conversation_view() {
            thread_view.read(cx).focus_handle(cx).focus(window, cx);
        }
    });
    cx.run_until_parked();

    sidebar.read_with(cx, |sidebar, _cx| {
        assert_active_thread(
            sidebar,
            &session_id_b2,
            "Focusing the agent panel thread should set focused_thread",
        );
        assert!(
            has_thread_entry(sidebar, &session_id_b2),
            "The focused thread should be present in the entries"
        );
    });
}

#[gpui::test]
async fn test_new_thread_button_works_after_adding_folder(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/project-a", cx).await;
    let fs = cx.update(|cx| <dyn fs::Fs>::global(cx));
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    // Start a thread and send a message so it has history.
    let connection = StubAgentConnection::new();
    connection.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
        acp_v1::ContentChunk::new("Done".into()),
    )]);
    open_thread_with_connection(&panel, connection, cx);
    send_message(&panel, cx);
    let session_id = active_session_id(&panel, cx);
    save_test_thread_metadata(&session_id, &project, cx).await;
    cx.run_until_parked();

    // Verify the thread appears in the sidebar.
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [project-a]",
            "  Hello *",
        ]
    );

    // The "New Thread" button should NOT be in "active/draft" state
    // because the panel has a thread with messages.
    sidebar.read_with(cx, |sidebar, _cx| {
        assert!(
            matches!(&sidebar.active_entry, Some(ActiveEntry::Thread { .. })),
            "Panel has a thread with messages, so active_entry should be Thread, got {:?}",
            sidebar.active_entry,
        );
    });

    // Now add a second folder to the workspace, changing the path_list.
    fs.as_fake()
        .insert_tree("/project-b", serde_json::json!({ "src": {} }))
        .await;
    project
        .update(cx, |project, cx| {
            project.find_or_create_worktree("/project-b", true, cx)
        })
        .await
        .expect("should add worktree");
    cx.run_until_parked();

    // The workspace path_list is now [project-a, project-b]. The active
    // thread's metadata was re-saved with the new paths by the agent panel's
    // project subscription. The old [project-a] key is replaced by the new
    // key since no other workspace claims it.
    let entries = visible_entries_as_strings(&sidebar, cx);
    // After adding a worktree, the thread migrates to the new group key.
    // A reconciliation draft may appear during the transition.
    assert!(
        entries.contains(&"  Hello *".to_string()),
        "thread should still be present after adding folder: {entries:?}"
    );
    assert_eq!(entries[0], "v [project-a, project-b]");

    // The "New Thread" button must still be clickable (not stuck in
    // "active/draft" state). Verify that `active_thread_is_draft` is
    // false — the panel still has the old thread with messages.
    sidebar.read_with(cx, |sidebar, _cx| {
        assert!(
            matches!(&sidebar.active_entry, Some(ActiveEntry::Thread { .. })),
            "After adding a folder the panel still has a thread with messages, \
                 so active_entry should be Thread, got {:?}",
            sidebar.active_entry,
        );
    });

    // Actually click "New Thread" by calling create_new_thread and
    // verify a new draft is created.
    let workspace = multi_workspace.read_with(cx, |mw, _cx| mw.workspace().clone());
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.create_new_thread(&workspace, window, cx);
    });
    cx.run_until_parked();

    // After creating a new thread, the panel should now be in draft
    // state (no messages on the new thread).
    sidebar.read_with(cx, |sidebar, _cx| {
        assert_active_draft(
            sidebar,
            &workspace,
            "After creating a new thread active_entry should be Draft",
        );
    });
}

#[gpui::test]
async fn test_draft_title_updates_from_editor_text(cx: &mut TestAppContext) {
    // When the user types into a draft, the parked draft entry's title in
    // the sidebar should reflect the editor's text — both while the
    // draft's `ConversationView` is still loaded (source: live message
    // editor) and after it has been evicted (source: kvp draft prompt
    // store, the same path used when drafts are restored from disk).
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    cx.run_until_parked();

    // Open an ephemeral draft via a stub connection so the conversation
    // view reaches Connected synchronously and the panel's draft_thread
    // pointer is populated.
    let connection = StubAgentConnection::new();
    agent_ui::test_support::open_draft_with_connection(&panel, connection, cx);
    cx.run_until_parked();
    let draft_id = panel.read_with(cx, |panel, cx| panel.active_thread_id(cx).unwrap());

    // Type into the (active) draft's message editor. The helper drains the
    // kvp-write debounce, so by the time it returns the prompt is on disk
    // — important for Phase 2 below, which exercises the kvp fallback.
    agent_ui::test_support::type_draft_prompt(&panel, "Fix the login bug", cx);

    // Park the draft by pressing Cmd-N while it has content.
    panel.update_in(cx, |panel, window, cx| {
        panel.new_thread(&NewThread, window, cx);
    });
    cx.run_until_parked();

    let draft_title = |sidebar: &Entity<Sidebar>, cx: &mut gpui::VisualTestContext| {
        sidebar.read_with(cx, |sidebar, _cx| {
            sidebar
                .contents
                .entries
                .iter()
                .find_map(|entry| match entry {
                    ListEntry::Thread(thread)
                        if thread.draft.is_some() && thread.metadata.thread_id == draft_id =>
                    {
                        Some(thread.metadata.display_title())
                    }
                    _ => None,
                })
                .expect("parked draft entry should be present")
        })
    };

    // Phase 1: ConversationView is still loaded in `retained_threads`;
    // the title comes from its live message editor.
    assert_eq!(
        draft_title(&sidebar, cx).as_ref(),
        "Fix the login bug",
        "parked draft title should match its editor text while loaded"
    );
    panel.read_with(cx, |panel, _cx| {
        assert!(
            panel.retained_threads().contains_key(&draft_id),
            "draft should be in retained_threads while loaded"
        );
    });

    // Phase 2: drop the draft's ConversationView from memory, mirroring
    // the state the sidebar sees immediately after a process restart
    // — the metadata row and the kvp draft prompt are on disk, but no
    // ConversationView has been rehydrated yet.
    let unloaded = panel.update(cx, |panel, _cx| panel.test_unload_retained_thread(draft_id));
    assert!(unloaded, "draft should have been present before unload");
    sidebar.update(cx, |sidebar, cx| sidebar.update_entries(cx));
    cx.run_until_parked();

    assert_eq!(
        draft_title(&sidebar, cx).as_ref(),
        "Fix the login bug",
        "parked draft title should still come from the kvp draft prompt store \
         even after its ConversationView is unloaded"
    );
}

#[gpui::test]
async fn test_thread_switcher_includes_parked_draft(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    cx.run_until_parked();

    save_thread_metadata(
        acp::SessionId::new(Arc::from("thread-existing")),
        Some("Existing Thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        None,
        None,
        &project,
        cx,
    );

    let connection = StubAgentConnection::new();
    agent_ui::test_support::open_draft_with_connection(&panel, connection, cx);
    cx.run_until_parked();
    let draft_id = panel.read_with(cx, |panel, cx| panel.active_thread_id(cx).unwrap());
    agent_ui::test_support::type_draft_prompt(&panel, "Fix the login bug", cx);

    panel.update_in(cx, |panel, window, cx| {
        panel.new_thread(&NewThread, window, cx);
    });
    cx.run_until_parked();

    sidebar.read_with(cx, |sidebar, _cx| {
        assert!(sidebar.contents.entries.iter().any(|entry| {
            matches!(entry, ListEntry::Thread(thread) if thread.metadata.thread_id == draft_id)
        }));
    });

    focus_sidebar(&sidebar, cx);
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.on_toggle_thread_switcher(&ToggleThreadSwitcher::default(), window, cx);
    });
    cx.run_until_parked();

    sidebar.read_with(cx, |sidebar, cx| {
        let switcher = sidebar
            .thread_switcher
            .as_ref()
            .expect("switcher should be open");
        assert!(switcher.read(cx).entries().iter().any(|entry| {
            matches!(entry.thread_id(), Some(thread_id) if thread_id == draft_id)
        }));
    });
}

#[gpui::test]
async fn test_plus_button_reuses_empty_draft(cx: &mut TestAppContext) {
    // Clicking `+` when an empty draft is already active should focus it
    // instead of creating and parking a new one.
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    cx.run_until_parked();

    // Open an initial draft against a stub so it connects synchronously.
    let connection = StubAgentConnection::new();
    agent_ui::test_support::open_draft_with_connection(&panel, connection, cx);
    cx.run_until_parked();

    let first_id = panel.read_with(cx, |panel, cx| {
        panel
            .active_thread_id(cx)
            .expect("draft should be active after open_draft_with_connection")
    });

    // Cmd-N with an empty draft should reuse it.
    panel.update_in(cx, |panel, window, cx| {
        panel.new_thread(&NewThread, window, cx);
    });
    cx.run_until_parked();

    let second_id = panel.read_with(cx, |panel, cx| {
        panel
            .active_thread_id(cx)
            .expect("draft should still be active after Cmd-N")
    });
    assert_eq!(
        first_id, second_id,
        "an empty draft should be reused, not replaced"
    );
    // The active empty draft is surfaced in the sidebar as a single
    // "New {agent} Thread" placeholder so the sidebar mirrors the panel.
    let draft_rows: Vec<_> = sidebar.read_with(cx, |sidebar, _| {
        sidebar
            .contents
            .entries
            .iter()
            .filter_map(|entry| match entry {
                ListEntry::Thread(t) if t.draft.is_some() => Some(t.clone()),
                _ => None,
            })
            .collect()
    });
    assert_eq!(
        draft_rows.len(),
        1,
        "active empty draft should appear as exactly one placeholder row"
    );
    assert_eq!(
        draft_rows[0].draft,
        Some(DraftKind::Empty),
        "the row should be the empty-draft placeholder"
    );
    assert_eq!(draft_rows[0].metadata.thread_id, first_id);
}

#[gpui::test]
async fn test_plus_button_parks_nonempty_draft(cx: &mut TestAppContext) {
    // Clicking `+` while the current draft has content should park the
    // current draft (surface it as a sidebar row) and create a new empty
    // draft as active.
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    cx.run_until_parked();

    // Open a draft via a stub so the ConversationView reaches Connected and
    // we can type into its editor.
    let connection = StubAgentConnection::new();
    agent_ui::test_support::open_draft_with_connection(&panel, connection, cx);
    cx.run_until_parked();
    let first_id = panel.read_with(cx, |panel, cx| panel.active_thread_id(cx).unwrap());
    let thread_view = panel.read_with(cx, |panel, cx| panel.active_thread_view(cx).unwrap());
    let editor = thread_view.read_with(cx, |view, _| view.message_editor.clone());
    editor.update_in(cx, |editor, window, cx| {
        editor.set_text("something the user typed", window, cx);
    });
    cx.run_until_parked();

    // Cmd-N parks the first draft and creates a new empty draft.
    panel.update_in(cx, |panel, window, cx| {
        panel.new_thread(&NewThread, window, cx);
    });
    cx.run_until_parked();

    let second_id = panel.read_with(cx, |panel, cx| panel.active_thread_id(cx).unwrap());
    assert_ne!(
        first_id, second_id,
        "non-empty draft should be parked and a fresh draft activated"
    );

    // Both drafts now appear as sidebar rows: the parked one with its
    // editor-derived title (real user state), and the newly-created empty
    // draft as a "New {agent} Thread" placeholder. The placeholder mirrors
    // the panel's current view; the parked row preserves typed content.
    let draft_rows: Vec<_> = sidebar.read_with(cx, |sidebar, _| {
        sidebar
            .contents
            .entries
            .iter()
            .filter_map(|entry| match entry {
                ListEntry::Thread(t) if t.draft.is_some() => Some(t.clone()),
                _ => None,
            })
            .collect()
    });
    assert_eq!(
        draft_rows.len(),
        2,
        "expected two draft rows (parked + new empty placeholder), got {:?}",
        draft_rows
            .iter()
            .map(|t| t.metadata.display_title())
            .collect::<Vec<_>>()
    );
    let parked = draft_rows
        .iter()
        .find(|t| t.metadata.thread_id == first_id)
        .expect("parked draft should be present");
    assert_eq!(
        parked.draft,
        Some(DraftKind::WithContent),
        "the parked draft has user content and is not an empty placeholder"
    );
    let new_empty = draft_rows
        .iter()
        .find(|t| t.metadata.thread_id == second_id)
        .expect("new empty draft should be present");
    assert_eq!(
        new_empty.draft,
        Some(DraftKind::Empty),
        "the freshly-created draft should be an empty placeholder"
    );
    assert_eq!(
        parked.metadata.display_title().as_ref(),
        "something the user typed"
    );

    // Reproduce the real-world inversion deterministically: parking
    // re-saves the filled draft, which can leave its display time newer
    // than the brand-new empty draft's. Force that here by pushing the
    // parked draft's `updated_at` into the future.
    cx.update(|_, cx| {
        let store = ThreadMetadataStore::global(cx);
        let mut parked_meta = store
            .read(cx)
            .entry(first_id)
            .expect("parked draft metadata should exist")
            .clone();
        parked_meta.interacted_at = None;
        parked_meta.updated_at = Utc::now() + chrono::Duration::hours(1);
        store.update(cx, |store, cx| store.save(parked_meta, cx));
    });
    cx.run_until_parked();

    // The empty-draft placeholder must still sort ABOVE the parked draft
    // despite the parked draft's newer timestamp — it's pinned to the top.
    let (empty_ix, parked_ix) = sidebar.read_with(cx, |sidebar, _| {
        let position = |id: ThreadId| {
            sidebar.contents.entries.iter().position(
                |entry| matches!(entry, ListEntry::Thread(t) if t.metadata.thread_id == id),
            )
        };
        (
            position(second_id).expect("empty draft row should be present"),
            position(first_id).expect("parked draft row should be present"),
        )
    });
    assert!(
        empty_ix < parked_ix,
        "the new empty draft (ix {empty_ix}) should sort above the parked filled draft (ix {parked_ix})"
    );
}

#[gpui::test]
async fn test_remove_draft_deletes_metadata_row(cx: &mut TestAppContext) {
    // The close-draft button deletes the metadata row and the kvp draft prompt,
    // and the draft disappears from the sidebar.
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    cx.run_until_parked();

    // Open a draft with content, park it by pressing Cmd-N.
    let connection = StubAgentConnection::new();
    agent_ui::test_support::open_draft_with_connection(&panel, connection, cx);
    cx.run_until_parked();
    let draft_id = panel.read_with(cx, |panel, cx| panel.active_thread_id(cx).unwrap());
    agent_ui::test_support::type_draft_prompt(&panel, "will be discarded", cx);
    panel.update_in(cx, |panel, window, cx| {
        panel.new_thread(&NewThread, window, cx);
    });
    cx.run_until_parked();

    // The parked draft is visible.
    let draft_index = sidebar.read_with(cx, |sidebar, _| {
        sidebar
            .contents
            .entries
            .iter()
            .position(|e| matches!(e, ListEntry::Thread(t) if t.metadata.thread_id == draft_id))
            .expect("parked draft should be visible before removal")
    });

    // Select the parked draft and dispatch the action a real user would
    // (Shift-Backspace, bound to `ArchiveSelectedThread`). The handler
    // routes to `remove_draft` for parked drafts.
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.selection = Some(draft_index);
        sidebar.archive_selected_thread(&agent_ui::ArchiveSelectedThread, window, cx);
    });
    cx.run_until_parked();

    // Metadata row and persisted draft prompt should both be gone.
    cx.update(|_window, cx| {
        let store = ThreadMetadataStore::global(cx).read(cx);
        assert!(
            store.entry(draft_id).is_none(),
            "removed draft metadata should be deleted"
        );
        assert!(
            agent_ui::draft_prompt_store::read(draft_id, cx).is_none(),
            "removed draft's kvp prompt should also be deleted"
        );
    });
    // And the row should be gone from the sidebar.
    let still_visible = sidebar.read_with(cx, |sidebar, _| {
        sidebar
            .contents
            .entries
            .iter()
            .any(|e| matches!(e, ListEntry::Thread(t) if t.metadata.thread_id == draft_id))
    });
    assert!(
        !still_visible,
        "removed draft should no longer appear in the sidebar"
    );
}

#[gpui::test]
async fn test_sending_message_from_draft_promotes_in_place(cx: &mut TestAppContext) {
    // Sending a message from a draft should keep the same ThreadId, set the
    // session_id on its metadata row, and clear the `draft_thread` pointer.
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (_sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    cx.run_until_parked();

    let connection = StubAgentConnection::new();
    connection.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
        acp_v1::ContentChunk::new("ok".into()),
    )]);
    open_thread_with_connection(&panel, connection, cx);
    let draft_id = panel.read_with(cx, |panel, cx| panel.active_thread_id(cx).unwrap());

    // Before sending: draft metadata row exists with session_id = None.
    cx.update(|_window, cx| {
        let store = ThreadMetadataStore::global(cx).read(cx);
        let entry = store.entry(draft_id).expect("draft metadata row");
        assert!(entry.is_draft(), "expected draft row before sending");
    });

    send_message(&panel, cx);
    cx.run_until_parked();

    // After sending: draft_thread is cleared, metadata row has a session_id.
    panel.read_with(cx, |panel, cx| {
        assert!(
            !panel.active_thread_is_draft(cx),
            "should no longer be a draft after send"
        );
        assert!(
            panel.ephemeral_draft_thread_id(cx).is_none(),
            "ephemeral draft pointer should be cleared after promotion"
        );
        assert_eq!(
            panel.active_thread_id(cx),
            Some(draft_id),
            "ThreadId stays the same across promotion"
        );
    });
    cx.update(|_window, cx| {
        let store = ThreadMetadataStore::global(cx).read(cx);
        let entry = store.entry(draft_id).expect("promoted metadata row");
        assert!(
            !entry.is_draft(),
            "promoted thread should have a session_id"
        );
    });
}

#[gpui::test]
async fn test_cmd_n_shows_new_thread_entry(cx: &mut TestAppContext) {
    // When the user presses Cmd-N (NewThread action) while viewing a
    // non-empty thread, the panel should switch to the draft thread and
    // the sidebar should surface a "New {agent} Thread" placeholder row
    // that mirrors the active empty draft.
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    // Create a non-empty thread (has messages).
    let connection = StubAgentConnection::new();
    connection.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
        acp_v1::ContentChunk::new("Done".into()),
    )]);
    open_thread_with_connection(&panel, connection, cx);
    send_message(&panel, cx);

    let session_id = active_session_id(&panel, cx);
    save_test_thread_metadata(&session_id, &project, cx).await;
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [my-project]",
            "  Hello *",
        ]
    );

    // Simulate cmd-n
    let workspace = multi_workspace.read_with(cx, |mw, _cx| mw.workspace().clone());
    panel.update_in(cx, |panel, window, cx| {
        panel.new_thread(&NewThread, window, cx);
    });
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.focus_panel::<AgentPanel>(window, cx);
    });
    cx.run_until_parked();

    // After Cmd-N the sidebar surfaces the active empty draft as a
    // placeholder row above the real thread.
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["v [my-project]", "  New stub Thread", "  Hello *"],
        "After Cmd-N the sidebar should show a placeholder row for the active empty draft"
    );

    // The panel should be on the draft and active_entry should track it.
    panel.read_with(cx, |panel, cx| {
        assert!(
            panel.active_thread_is_draft(cx),
            "panel should be showing the draft after Cmd-N",
        );
    });
    sidebar.read_with(cx, |sidebar, _cx| {
        assert_active_draft(
            sidebar,
            &workspace,
            "active_entry should be Draft after Cmd-N",
        );
    });
}
async fn init_test_project_with_git(
    worktree_path: &str,
    cx: &mut TestAppContext,
) -> (Entity<project::Project>, Arc<dyn fs::Fs>) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        worktree_path,
        serde_json::json!({
            ".git": {},
            "src": {},
        }),
    )
    .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
    let project = project::Project::test(fs.clone(), [worktree_path.as_ref()], cx).await;
    (project, fs)
}
// Reproduces the core of the user-reported bug: a thread belonging to
// a multi-root workspace that mixes a standalone project and a linked
// git worktree can become invisible in the sidebar when its stored
// `main_worktree_paths` don't match the workspace's project group
// key. The metadata still exists and Thread History still shows it,
// but the sidebar rebuild's lookups all miss.
//
// Real-world setup: a single multi-root workspace whose roots are
// `[/cloud, /worktrees/zed/wt_a/zed]`, where:
//   - `/cloud` is a standalone git repo (main == folder).
//   - `/worktrees/zed/wt_a/zed` is a linked worktree of `/zed`.
//
// Once git scans complete the project group key is
// `[/cloud, /zed]` — the main paths of the two roots. A thread
// created in this workspace is written with
// `main=[/cloud, /zed], folder=[/cloud, /worktrees/zed/wt_a/zed]`
// and the sidebar finds it via `entries_for_main_worktree_path`.
//
// If some other code path (stale data on reload, a path-less archive
// restored via the project picker, a legacy write …) persists the
// thread with `main == folder` instead, the stored
// `main_worktree_paths` is
// `[/cloud, /worktrees/zed/wt_a/zed]` ≠ `[/cloud, /zed]`. The three
// lookups in `rebuild_contents` all miss:
//
//   1. `entries_for_main_worktree_path([/cloud, /zed])` — the
//      thread's stored main doesn't equal the group key.
//   2. `entries_for_path([/cloud, /zed])` — the thread's folder paths
//      don't equal the group key either.
//   3. The linked-worktree fallback iterates the group's workspaces'
//      `linked_worktrees()` snapshots. Those yield *sibling* linked
//      worktrees of the repo, not the workspace's own roots, so the
//      thread's folder `/worktrees/zed/wt_a/zed` doesn't match.
//
// The row falls out of the sidebar entirely — matching the user's
// symptom of a thread visible in the agent panel but missing from
// the sidebar. It only reappears once something re-writes the
// thread's metadata in the good shape (e.g. `handle_conversation_event`
// firing after the user sends a message).
//
// We directly persist the bad shape via `store.save(...)` rather
// than trying to reproduce the original writer. The bug is
// ultimately about the sidebar's tolerance for any stale row whose
// folder paths correspond to an open workspace's roots, regardless
// of how that row came to be in the store.
#[gpui::test]
async fn test_activate_archived_thread_with_saved_paths_activates_matching_workspace(
    cx: &mut TestAppContext,
) {
    // Thread has saved metadata in ThreadStore. A matching workspace is
    // already open. Expected: activates the matching workspace.
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/project-a", serde_json::json!({ "src": {} }))
        .await;
    fs.insert_tree("/project-b", serde_json::json!({ "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let project_a = project::Project::test(fs.clone(), ["/project-a".as_ref()], cx).await;
    let project_b = project::Project::test(fs.clone(), ["/project-b".as_ref()], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));

    let sidebar = setup_sidebar(&multi_workspace, cx);

    let workspace_b = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(project_b.clone(), window, cx)
    });
    let workspace_a =
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().next().unwrap().clone());

    // Save a thread with path_list pointing to project-b.
    let session_id = acp::SessionId::new(Arc::from("archived-1"));
    save_test_thread_metadata(&session_id, &project_b, cx).await;

    // Ensure workspace A is active.
    multi_workspace.update_in(cx, |mw, window, cx| {
        let workspace = mw.workspaces().next().unwrap().clone();
        mw.activate(workspace, None, window, cx);
    });
    cx.run_until_parked();
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspace().clone()),
        workspace_a
    );

    // Call activate_archived_thread – should resolve saved paths and
    // switch to the workspace for project-b.
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.open_thread_from_archive(
            ThreadMetadata {
                thread_id: ThreadId::new(),
                session_id: Some(session_id.clone()),
                agent_id: agent::ZED_AGENT_ID.clone(),
                title: Some("Archived Thread".into()),
                title_override: None,
                updated_at: Utc::now(),
                created_at: None,
                interacted_at: None,
                worktree_paths: WorktreePaths::from_folder_paths(&PathList::new(&[PathBuf::from(
                    "/project-b",
                )])),
                archived: false,
                remote_connection: None,
            },
            window,
            cx,
        );
    });
    cx.run_until_parked();

    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspace().clone()),
        workspace_b,
        "should have switched to the workspace matching the saved paths"
    );
}

#[gpui::test]
async fn test_activate_archived_thread_cwd_fallback_with_matching_workspace(
    cx: &mut TestAppContext,
) {
    // Thread has no saved metadata but session_info has cwd. A matching
    // workspace is open. Expected: uses cwd to find and activate it.
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/project-a", serde_json::json!({ "src": {} }))
        .await;
    fs.insert_tree("/project-b", serde_json::json!({ "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let project_a = project::Project::test(fs.clone(), ["/project-a".as_ref()], cx).await;
    let project_b = project::Project::test(fs.clone(), ["/project-b".as_ref()], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a, window, cx));

    let sidebar = setup_sidebar(&multi_workspace, cx);

    let workspace_b = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(project_b, window, cx)
    });
    let workspace_a =
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().next().unwrap().clone());

    // Start with workspace A active.
    multi_workspace.update_in(cx, |mw, window, cx| {
        let workspace = mw.workspaces().next().unwrap().clone();
        mw.activate(workspace, None, window, cx);
    });
    cx.run_until_parked();
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspace().clone()),
        workspace_a
    );

    // No thread saved to the store – cwd is the only path hint.
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.open_thread_from_archive(
            ThreadMetadata {
                thread_id: ThreadId::new(),
                session_id: Some(acp::SessionId::new(Arc::from("unknown-session"))),
                agent_id: agent::ZED_AGENT_ID.clone(),
                title: Some("CWD Thread".into()),
                title_override: None,
                updated_at: Utc::now(),
                created_at: None,
                interacted_at: None,
                worktree_paths: WorktreePaths::from_folder_paths(&PathList::new(&[
                    std::path::PathBuf::from("/project-b"),
                ])),
                archived: false,
                remote_connection: None,
            },
            window,
            cx,
        );
    });
    cx.run_until_parked();

    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspace().clone()),
        workspace_b,
        "should have activated the workspace matching the cwd"
    );
}

#[gpui::test]
async fn test_activate_archived_thread_no_paths_no_cwd_uses_active_workspace(
    cx: &mut TestAppContext,
) {
    // Thread has no saved metadata and no cwd. Expected: falls back to
    // the currently active workspace.
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/project-a", serde_json::json!({ "src": {} }))
        .await;
    fs.insert_tree("/project-b", serde_json::json!({ "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let project_a = project::Project::test(fs.clone(), ["/project-a".as_ref()], cx).await;
    let project_b = project::Project::test(fs.clone(), ["/project-b".as_ref()], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a, window, cx));

    let sidebar = setup_sidebar(&multi_workspace, cx);

    let workspace_b = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(project_b, window, cx)
    });

    // Activate workspace B (index 1) to make it the active one.
    multi_workspace.update_in(cx, |mw, window, cx| {
        let workspace = mw.workspaces().nth(1).unwrap().clone();
        mw.activate(workspace, None, window, cx);
    });
    cx.run_until_parked();
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspace().clone()),
        workspace_b
    );

    // No saved thread, no cwd – should fall back to the active workspace.
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.open_thread_from_archive(
            ThreadMetadata {
                thread_id: ThreadId::new(),
                session_id: Some(acp::SessionId::new(Arc::from("no-context-session"))),
                agent_id: agent::ZED_AGENT_ID.clone(),
                title: Some("Contextless Thread".into()),
                title_override: None,
                updated_at: Utc::now(),
                created_at: None,
                interacted_at: None,
                worktree_paths: WorktreePaths::default(),
                archived: false,
                remote_connection: None,
            },
            window,
            cx,
        );
    });
    cx.run_until_parked();

    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspace().clone()),
        workspace_b,
        "should have stayed on the active workspace when no path info is available"
    );
}

#[gpui::test]
async fn test_activate_archived_thread_saved_paths_opens_new_workspace(cx: &mut TestAppContext) {
    // Thread has saved metadata pointing to a path with no open workspace.
    // Expected: opens a new workspace for that path.
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/project-a", serde_json::json!({ "src": {} }))
        .await;
    fs.insert_tree("/project-b", serde_json::json!({ "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let project_a = project::Project::test(fs.clone(), ["/project-a".as_ref()], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a, window, cx));

    let sidebar = setup_sidebar(&multi_workspace, cx);

    // Save a thread with path_list pointing to project-b – which has no
    // open workspace.
    let path_list_b = PathList::new(&[std::path::PathBuf::from("/project-b")]);
    let session_id = acp::SessionId::new(Arc::from("archived-new-ws"));

    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        1,
        "should start with one workspace"
    );

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.open_thread_from_archive(
            ThreadMetadata {
                thread_id: ThreadId::new(),
                session_id: Some(session_id.clone()),
                agent_id: agent::ZED_AGENT_ID.clone(),
                title: Some("New WS Thread".into()),
                title_override: None,
                updated_at: Utc::now(),
                created_at: None,
                interacted_at: None,
                worktree_paths: WorktreePaths::from_folder_paths(&path_list_b),
                archived: false,
                remote_connection: None,
            },
            window,
            cx,
        );
    });
    cx.run_until_parked();

    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        2,
        "should have opened a second workspace for the archived thread's saved paths"
    );
}

#[gpui::test]
async fn test_activate_archived_thread_reuses_workspace_in_another_window(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/project-a", serde_json::json!({ "src": {} }))
        .await;
    fs.insert_tree("/project-b", serde_json::json!({ "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let project_a = project::Project::test(fs.clone(), ["/project-a".as_ref()], cx).await;
    let project_b = project::Project::test(fs.clone(), ["/project-b".as_ref()], cx).await;

    let multi_workspace_a =
        cx.add_window(|window, cx| MultiWorkspace::test_new(project_a, window, cx));
    let multi_workspace_b =
        cx.add_window(|window, cx| MultiWorkspace::test_new(project_b, window, cx));

    let multi_workspace_a_entity = multi_workspace_a.root(cx).unwrap();
    let multi_workspace_b_entity = multi_workspace_b.root(cx).unwrap();

    let cx_b = &mut gpui::VisualTestContext::from_window(multi_workspace_b.into(), cx);
    let _sidebar_b = setup_sidebar(&multi_workspace_b_entity, cx_b);

    let cx_a = &mut gpui::VisualTestContext::from_window(multi_workspace_a.into(), cx);
    let sidebar = setup_sidebar(&multi_workspace_a_entity, cx_a);

    let session_id = acp::SessionId::new(Arc::from("archived-cross-window"));

    sidebar.update_in(cx_a, |sidebar, window, cx| {
        sidebar.open_thread_from_archive(
            ThreadMetadata {
                thread_id: ThreadId::new(),
                session_id: Some(session_id.clone()),
                agent_id: agent::ZED_AGENT_ID.clone(),
                title: Some("Cross Window Thread".into()),
                title_override: None,
                updated_at: Utc::now(),
                created_at: None,
                interacted_at: None,
                worktree_paths: WorktreePaths::from_folder_paths(&PathList::new(&[PathBuf::from(
                    "/project-b",
                )])),
                archived: false,
                remote_connection: None,
            },
            window,
            cx,
        );
    });
    cx_a.run_until_parked();

    assert_eq!(
        multi_workspace_a
            .read_with(cx_a, |mw, _| mw.workspaces().count())
            .unwrap(),
        1,
        "should not add the other window's workspace into the current window"
    );
    assert_eq!(
        multi_workspace_b
            .read_with(cx_a, |mw, _| mw.workspaces().count())
            .unwrap(),
        1,
        "should reuse the existing workspace in the other window"
    );
    assert!(
        cx_a.read(|cx| cx.active_window().unwrap()) == *multi_workspace_b,
        "should activate the window that already owns the matching workspace"
    );
    sidebar.read_with(cx_a, |sidebar, _| {
            assert!(
                !is_active_session(&sidebar, &session_id),
                "source window's sidebar should not eagerly claim focus for a thread opened in another window"
            );
        });
}

#[gpui::test]
async fn test_activate_archived_thread_reuses_workspace_in_another_window_with_target_sidebar(
    cx: &mut TestAppContext,
) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/project-a", serde_json::json!({ "src": {} }))
        .await;
    fs.insert_tree("/project-b", serde_json::json!({ "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let project_a = project::Project::test(fs.clone(), ["/project-a".as_ref()], cx).await;
    let project_b = project::Project::test(fs.clone(), ["/project-b".as_ref()], cx).await;

    let multi_workspace_a =
        cx.add_window(|window, cx| MultiWorkspace::test_new(project_a, window, cx));
    let multi_workspace_b =
        cx.add_window(|window, cx| MultiWorkspace::test_new(project_b.clone(), window, cx));

    let multi_workspace_a_entity = multi_workspace_a.root(cx).unwrap();
    let multi_workspace_b_entity = multi_workspace_b.root(cx).unwrap();

    let cx_a = &mut gpui::VisualTestContext::from_window(multi_workspace_a.into(), cx);
    let sidebar_a = setup_sidebar(&multi_workspace_a_entity, cx_a);

    let cx_b = &mut gpui::VisualTestContext::from_window(multi_workspace_b.into(), cx);
    let sidebar_b = setup_sidebar(&multi_workspace_b_entity, cx_b);
    let workspace_b = multi_workspace_b_entity.read_with(cx_b, |mw, _| mw.workspace().clone());
    let _panel_b = add_agent_panel(&workspace_b, cx_b);

    let session_id = acp::SessionId::new(Arc::from("archived-cross-window-with-sidebar"));
    let metadata = ThreadMetadata {
        thread_id: ThreadId::new(),
        session_id: Some(session_id.clone()),
        agent_id: agent::ZED_AGENT_ID.clone(),
        title: Some("Cross Window Thread".into()),
        title_override: None,
        updated_at: Utc::now(),
        created_at: None,
        interacted_at: None,
        worktree_paths: WorktreePaths::from_folder_paths(&PathList::new(&[PathBuf::from(
            "/project-b",
        )])),
        archived: false,
        remote_connection: None,
    };
    seed_thread_metadata(metadata.clone(), cx_a);

    sidebar_a.update_in(cx_a, |sidebar, window, cx| {
        sidebar.open_thread_from_archive(metadata, window, cx);
    });
    cx_a.run_until_parked();

    assert_eq!(
        multi_workspace_a
            .read_with(cx_a, |mw, _| mw.workspaces().count())
            .unwrap(),
        1,
        "should not add the other window's workspace into the current window"
    );
    assert_eq!(
        multi_workspace_b
            .read_with(cx_a, |mw, _| mw.workspaces().count())
            .unwrap(),
        1,
        "should reuse the existing workspace in the other window"
    );
    assert!(
        cx_a.read(|cx| cx.active_window().unwrap()) == *multi_workspace_b,
        "should activate the window that already owns the matching workspace"
    );
    sidebar_a.read_with(cx_a, |sidebar, _| {
            assert!(
                !is_active_session(&sidebar, &session_id),
                "source window's sidebar should not eagerly claim focus for a thread opened in another window"
            );
        });
    sidebar_b.read_with(cx_b, |sidebar, _| {
        assert_active_thread(
            sidebar,
            &session_id,
            "target window's sidebar should eagerly focus the activated archived thread",
        );
    });
}

#[gpui::test]
async fn test_activate_archived_thread_prefers_current_window_for_matching_paths(
    cx: &mut TestAppContext,
) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/project-a", serde_json::json!({ "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let project_b = project::Project::test(fs.clone(), ["/project-a".as_ref()], cx).await;
    let project_a = project::Project::test(fs.clone(), ["/project-a".as_ref()], cx).await;

    let multi_workspace_b =
        cx.add_window(|window, cx| MultiWorkspace::test_new(project_b, window, cx));
    let multi_workspace_a =
        cx.add_window(|window, cx| MultiWorkspace::test_new(project_a, window, cx));

    let multi_workspace_a_entity = multi_workspace_a.root(cx).unwrap();
    let multi_workspace_b_entity = multi_workspace_b.root(cx).unwrap();

    let cx_b = &mut gpui::VisualTestContext::from_window(multi_workspace_b.into(), cx);
    let _sidebar_b = setup_sidebar(&multi_workspace_b_entity, cx_b);

    let cx_a = &mut gpui::VisualTestContext::from_window(multi_workspace_a.into(), cx);
    let sidebar_a = setup_sidebar(&multi_workspace_a_entity, cx_a);

    let session_id = acp::SessionId::new(Arc::from("archived-current-window"));
    let metadata = ThreadMetadata {
        thread_id: ThreadId::new(),
        session_id: Some(session_id.clone()),
        agent_id: agent::ZED_AGENT_ID.clone(),
        title: Some("Current Window Thread".into()),
        title_override: None,
        updated_at: Utc::now(),
        created_at: None,
        interacted_at: None,
        worktree_paths: WorktreePaths::from_folder_paths(&PathList::new(&[PathBuf::from(
            "/project-a",
        )])),
        archived: false,
        remote_connection: None,
    };
    seed_thread_metadata(metadata.clone(), cx_a);

    sidebar_a.update_in(cx_a, |sidebar, window, cx| {
        sidebar.open_thread_from_archive(metadata, window, cx);
    });
    cx_a.run_until_parked();

    assert!(
        cx_a.read(|cx| cx.active_window().unwrap()) == *multi_workspace_a,
        "should keep activation in the current window when it already has a matching workspace"
    );
    sidebar_a.read_with(cx_a, |sidebar, _| {
        assert_active_thread(
            sidebar,
            &session_id,
            "current window's sidebar should eagerly focus the activated archived thread",
        );
    });
    assert_eq!(
        multi_workspace_a
            .read_with(cx_a, |mw, _| mw.workspaces().count())
            .unwrap(),
        1,
        "current window should continue reusing its existing workspace"
    );
    assert_eq!(
        multi_workspace_b
            .read_with(cx_a, |mw, _| mw.workspaces().count())
            .unwrap(),
        1,
        "other windows should not be activated just because they also match the saved paths"
    );
}
fn thread_id_for(session_id: &acp::SessionId, cx: &mut TestAppContext) -> ThreadId {
    cx.read(|cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry_by_session(session_id)
            .map(|m| m.thread_id)
            .expect("thread metadata should exist")
    })
}

#[gpui::test]
async fn test_thread_switcher_ordering(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    let switcher_ids =
        |sidebar: &Entity<Sidebar>, cx: &mut gpui::VisualTestContext| -> Vec<ThreadId> {
            sidebar.read_with(cx, |sidebar, cx| {
                let switcher = sidebar
                    .thread_switcher
                    .as_ref()
                    .expect("switcher should be open");
                switcher
                    .read(cx)
                    .entries()
                    .iter()
                    .map(|entry| entry.thread_id().expect("expected thread switcher entry"))
                    .collect()
            })
        };

    let switcher_selected_id =
        |sidebar: &Entity<Sidebar>, cx: &mut gpui::VisualTestContext| -> ThreadId {
            sidebar.read_with(cx, |sidebar, cx| {
                let switcher = sidebar
                    .thread_switcher
                    .as_ref()
                    .expect("switcher should be open");
                let s = switcher.read(cx);
                s.selected_entry()
                    .expect("should have selection")
                    .thread_id()
                    .expect("expected selected thread entry")
            })
        };

    // ── Setup: create three threads with distinct created_at times ──────
    // Thread C (oldest), Thread B, Thread A (newest) — by created_at.
    // We send messages in each so they also get last_message_sent_or_queued timestamps.
    let connection_c = StubAgentConnection::new();
    connection_c.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
        acp_v1::ContentChunk::new("Done C".into()),
    )]);
    open_thread_with_connection(&panel, connection_c, cx);
    send_message(&panel, cx);
    let session_id_c = active_session_id(&panel, cx);
    let thread_id_c = active_thread_id(&panel, cx);
    save_thread_metadata(
        session_id_c.clone(),
        Some("Thread C".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        Some(chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap()),
        None,
        &project,
        cx,
    );

    let connection_b = StubAgentConnection::new();
    connection_b.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
        acp_v1::ContentChunk::new("Done B".into()),
    )]);
    open_thread_with_connection(&panel, connection_b, cx);
    send_message(&panel, cx);
    let session_id_b = active_session_id(&panel, cx);
    let thread_id_b = active_thread_id(&panel, cx);
    save_thread_metadata(
        session_id_b.clone(),
        Some("Thread B".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 2, 0, 0, 0).unwrap(),
        Some(chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 2, 0, 0, 0).unwrap()),
        None,
        &project,
        cx,
    );

    let connection_a = StubAgentConnection::new();
    connection_a.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
        acp_v1::ContentChunk::new("Done A".into()),
    )]);
    open_thread_with_connection(&panel, connection_a, cx);
    send_message(&panel, cx);
    let session_id_a = active_session_id(&panel, cx);
    let thread_id_a = active_thread_id(&panel, cx);
    save_thread_metadata(
        session_id_a.clone(),
        Some("Thread A".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 3, 0, 0, 0).unwrap(),
        Some(chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 3, 0, 0, 0).unwrap()),
        None,
        &project,
        cx,
    );

    // All three threads are now live. Thread A was opened last, so it's
    // the one being viewed. Opening each thread called record_thread_access,
    // so all three have last_accessed_at set.
    // Access order is: A (most recent), B, C (oldest).

    // ── 1. Open switcher: threads sorted by last_accessed_at ─────────────────
    focus_sidebar(&sidebar, cx);
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.on_toggle_thread_switcher(&ToggleThreadSwitcher::default(), window, cx);
    });
    cx.run_until_parked();

    // All three have last_accessed_at, so they sort by access time.
    // A was accessed most recently (it's the currently viewed thread),
    // then B, then C.
    assert_eq!(
        switcher_ids(&sidebar, cx),
        vec![thread_id_a, thread_id_b, thread_id_c,],
    );
    // First ctrl-tab selects the second entry (B).
    assert_eq!(switcher_selected_id(&sidebar, cx), thread_id_b);

    // Dismiss the switcher without confirming.
    sidebar.update_in(cx, |sidebar, _window, cx| {
        sidebar.dismiss_thread_switcher(cx);
    });
    cx.run_until_parked();

    // ── 2. Confirm on Thread C: it becomes most-recently-accessed ──────
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.on_toggle_thread_switcher(&ToggleThreadSwitcher::default(), window, cx);
    });
    cx.run_until_parked();

    // Cycle twice to land on Thread C (index 2).
    sidebar.read_with(cx, |sidebar, cx| {
        let switcher = sidebar.thread_switcher.as_ref().unwrap();
        assert_eq!(switcher.read(cx).selected_index(), 1);
    });
    sidebar.update_in(cx, |sidebar, _window, cx| {
        sidebar
            .thread_switcher
            .as_ref()
            .unwrap()
            .update(cx, |s, cx| s.cycle_selection(cx));
    });
    cx.run_until_parked();
    assert_eq!(switcher_selected_id(&sidebar, cx), thread_id_c);

    assert!(sidebar.update(cx, |sidebar, _cx| sidebar.thread_last_accessed.is_empty()));

    // Confirm on Thread C.
    sidebar.update_in(cx, |sidebar, window, cx| {
        let switcher = sidebar.thread_switcher.as_ref().unwrap();
        let focus = switcher.focus_handle(cx);
        focus.dispatch_action(&menu::Confirm, window, cx);
    });
    cx.run_until_parked();

    // Switcher should be dismissed after confirm.
    sidebar.read_with(cx, |sidebar, _cx| {
        assert!(
            sidebar.thread_switcher.is_none(),
            "switcher should be dismissed"
        );
    });

    sidebar.update(cx, |sidebar, _cx| {
        let last_accessed = sidebar
            .thread_last_accessed
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(last_accessed.len(), 1);
        assert!(last_accessed.contains(&thread_id_c));
        assert!(
            is_active_session(&sidebar, &session_id_c),
            "active_entry should be Thread({session_id_c:?})"
        );
    });

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.on_toggle_thread_switcher(&ToggleThreadSwitcher::default(), window, cx);
    });
    cx.run_until_parked();

    assert_eq!(
        switcher_ids(&sidebar, cx),
        vec![thread_id_c, thread_id_a, thread_id_b],
    );

    // Confirm on Thread A.
    sidebar.update_in(cx, |sidebar, window, cx| {
        let switcher = sidebar.thread_switcher.as_ref().unwrap();
        let focus = switcher.focus_handle(cx);
        focus.dispatch_action(&menu::Confirm, window, cx);
    });
    cx.run_until_parked();

    sidebar.update(cx, |sidebar, _cx| {
        let last_accessed = sidebar
            .thread_last_accessed
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(last_accessed.len(), 2);
        assert!(last_accessed.contains(&thread_id_c));
        assert!(last_accessed.contains(&thread_id_a));
        assert!(
            is_active_session(&sidebar, &session_id_a),
            "active_entry should be Thread({session_id_a:?})"
        );
    });

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.on_toggle_thread_switcher(&ToggleThreadSwitcher::default(), window, cx);
    });
    cx.run_until_parked();

    assert_eq!(
        switcher_ids(&sidebar, cx),
        vec![thread_id_a, thread_id_c, thread_id_b,],
    );

    sidebar.update_in(cx, |sidebar, _window, cx| {
        let switcher = sidebar.thread_switcher.as_ref().unwrap();
        switcher.update(cx, |switcher, cx| switcher.cycle_selection(cx));
    });
    cx.run_until_parked();

    // Confirm on Thread B.
    sidebar.update_in(cx, |sidebar, window, cx| {
        let switcher = sidebar.thread_switcher.as_ref().unwrap();
        let focus = switcher.focus_handle(cx);
        focus.dispatch_action(&menu::Confirm, window, cx);
    });
    cx.run_until_parked();

    sidebar.update(cx, |sidebar, _cx| {
        let last_accessed = sidebar
            .thread_last_accessed
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(last_accessed.len(), 3);
        assert!(last_accessed.contains(&thread_id_c));
        assert!(last_accessed.contains(&thread_id_a));
        assert!(last_accessed.contains(&thread_id_b));
        assert!(
            is_active_session(&sidebar, &session_id_b),
            "active_entry should be Thread({session_id_b:?})"
        );
    });

    // ── 3. Add a historical thread (no last_accessed_at, no message sent) ──
    // This thread was never opened in a panel — it only exists in metadata.
    save_thread_metadata(
        acp::SessionId::new(Arc::from("thread-historical")),
        Some("Historical Thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 6, 1, 0, 0, 0).unwrap(),
        Some(chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 6, 1, 0, 0, 0).unwrap()),
        None,
        &project,
        cx,
    );

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.on_toggle_thread_switcher(&ToggleThreadSwitcher::default(), window, cx);
    });
    cx.run_until_parked();

    // Historical Thread has no last_accessed_at and no last_message_sent_or_queued,
    // so it falls to tier 3 (sorted by created_at). It should appear after all
    // accessed threads, even though its created_at (June 2024) is much later
    // than the others.
    //
    // But the live threads (A, B, C) each had send_message called which sets
    // last_message_sent_or_queued. So for the accessed threads (tier 1) the
    // sort key is last_accessed_at; for Historical Thread (tier 3) it's created_at.
    let session_id_hist = acp::SessionId::new(Arc::from("thread-historical"));
    let thread_id_hist = thread_id_for(&session_id_hist, cx);

    let ids = switcher_ids(&sidebar, cx);
    assert_eq!(
        ids,
        vec![thread_id_b, thread_id_a, thread_id_c, thread_id_hist],
    );

    sidebar.update_in(cx, |sidebar, _window, cx| {
        sidebar.dismiss_thread_switcher(cx);
    });
    cx.run_until_parked();

    // ── 4. Add another historical thread with older created_at ─────────
    save_thread_metadata(
        acp::SessionId::new(Arc::from("thread-old-historical")),
        Some("Old Historical Thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2023, 6, 1, 0, 0, 0).unwrap(),
        Some(chrono::TimeZone::with_ymd_and_hms(&Utc, 2023, 6, 1, 0, 0, 0).unwrap()),
        None,
        &project,
        cx,
    );

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.on_toggle_thread_switcher(&ToggleThreadSwitcher::default(), window, cx);
    });
    cx.run_until_parked();

    // Both historical threads have no access or message times. They should
    // appear after accessed threads, sorted by created_at (newest first).
    let session_id_old_hist = acp::SessionId::new(Arc::from("thread-old-historical"));
    let thread_id_old_hist = thread_id_for(&session_id_old_hist, cx);
    let ids = switcher_ids(&sidebar, cx);
    assert_eq!(
        ids,
        vec![
            thread_id_b,
            thread_id_a,
            thread_id_c,
            thread_id_hist,
            thread_id_old_hist,
        ],
    );

    sidebar.update_in(cx, |sidebar, _window, cx| {
        sidebar.dismiss_thread_switcher(cx);
    });
    cx.run_until_parked();
}

#[gpui::test]
async fn test_archive_thread_keeps_metadata_but_hides_from_sidebar(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_thread_metadata(
        acp::SessionId::new(Arc::from("thread-to-archive")),
        Some("Thread To Archive".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        None,
        None,
        &project,
        cx,
    );
    cx.run_until_parked();

    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    let entries = visible_entries_as_strings(&sidebar, cx);
    assert!(
        entries.iter().any(|e| e.contains("Thread To Archive")),
        "expected thread to be visible before archiving, got: {entries:?}"
    );

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.archive_thread(
            &acp::SessionId::new(Arc::from("thread-to-archive")),
            window,
            cx,
        );
    });
    cx.run_until_parked();

    let entries = visible_entries_as_strings(&sidebar, cx);
    assert!(
        !entries.iter().any(|e| e.contains("Thread To Archive")),
        "expected thread to be hidden after archiving, got: {entries:?}"
    );

    cx.update(|_, cx| {
        let store = ThreadMetadataStore::global(cx);
        let archived: Vec<_> = store.read(cx).archived_entries().collect();
        assert_eq!(archived.len(), 1);
        assert_eq!(
            archived[0].session_id.as_ref().unwrap().0.as_ref(),
            "thread-to-archive"
        );
        assert!(archived[0].archived);
    });
}

#[gpui::test]
async fn test_archive_thread_drops_retained_conversation_view(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/project-a", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    cx.run_until_parked();

    let connection = acp_thread::StubAgentConnection::new();
    connection.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
        acp_v1::ContentChunk::new("Done".into()),
    )]);
    open_thread_with_connection(&panel, connection, cx);
    send_message(&panel, cx);
    let session_id = active_session_id(&panel, cx);
    let thread_id = active_thread_id(&panel, cx);
    cx.run_until_parked();

    sidebar.read_with(cx, |sidebar, _| {
        assert!(
            is_active_session(sidebar, &session_id),
            "expected the newly created thread to be active before archiving",
        );
    });

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.archive_thread(&session_id, window, cx);
    });
    cx.run_until_parked();

    panel.read_with(cx, |panel, _| {
        assert!(
            !panel.is_retained_thread(&thread_id),
            "archiving a thread must drop its ConversationView from retained_threads, \
             but the archived thread id {thread_id:?} is still retained",
        );
    });
}

#[gpui::test]
async fn test_archive_thread_active_entry_management(cx: &mut TestAppContext) {
    // Tests two archive scenarios:
    // 1. Archiving a thread in a non-active workspace leaves active_entry
    //    as the current draft.
    // 2. Archiving the thread the user is looking at falls back to a draft
    //    on the same workspace.
    agent_ui::test_support::init_test(cx);
    cx.update(|cx| {
        ThreadStore::init_global(cx);
        ThreadMetadataStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
        prompt_store::init(cx);
    });

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/project-a", serde_json::json!({ "src": {} }))
        .await;
    fs.insert_tree("/project-b", serde_json::json!({ "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let project_a = project::Project::test(fs.clone(), ["/project-a".as_ref()], cx).await;
    let project_b = project::Project::test(fs.clone(), ["/project-b".as_ref()], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));
    let (sidebar, panel_a) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    let workspace_b = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(project_b.clone(), window, cx)
    });
    let panel_b = add_agent_panel(&workspace_b, cx);
    cx.run_until_parked();

    // Explicitly create a draft on workspace_b so the sidebar tracks one.
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.create_new_thread(&workspace_b, window, cx);
    });
    cx.run_until_parked();

    // --- Scenario 1: archive a thread in the non-active workspace ---

    // Create a thread in project-a (non-active — project-b is active).
    let connection = acp_thread::StubAgentConnection::new();
    connection.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
        acp_v1::ContentChunk::new("Done".into()),
    )]);
    agent_ui::test_support::open_thread_with_connection(&panel_a, connection, cx);
    agent_ui::test_support::send_message(&panel_a, cx);
    let thread_a = agent_ui::test_support::active_session_id(&panel_a, cx);
    cx.run_until_parked();

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.archive_thread(&thread_a, window, cx);
    });
    cx.run_until_parked();

    // active_entry should still be a draft on workspace_b (the active one).
    sidebar.read_with(cx, |sidebar, _| {
        assert!(
            matches!(&sidebar.active_entry, Some(ActiveEntry::Thread { workspace: ws, .. }) if ws == &workspace_b),
            "expected Draft(workspace_b) after archiving non-active thread, got: {:?}",
            sidebar.active_entry,
        );
    });

    // --- Scenario 2: archive the thread the user is looking at ---

    // Create a thread in project-b (the active workspace) and verify it
    // becomes the active entry.
    let connection = acp_thread::StubAgentConnection::new();
    connection.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
        acp_v1::ContentChunk::new("Done".into()),
    )]);
    agent_ui::test_support::open_thread_with_connection(&panel_b, connection, cx);
    agent_ui::test_support::send_message(&panel_b, cx);
    let thread_b = agent_ui::test_support::active_session_id(&panel_b, cx);
    cx.run_until_parked();

    sidebar.read_with(cx, |sidebar, _| {
        assert!(
            is_active_session(&sidebar, &thread_b),
            "expected active_entry to be Thread({thread_b}), got: {:?}",
            sidebar.active_entry,
        );
    });

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.archive_thread(&thread_b, window, cx);
    });
    cx.run_until_parked();

    // Archiving the active thread activates a draft on the same workspace
    // (via clear_base_view → activate_draft). The draft is not shown as a
    // sidebar row but active_entry tracks it.
    sidebar.read_with(cx, |sidebar, _| {
        assert!(
            matches!(&sidebar.active_entry, Some(ActiveEntry::Thread { workspace: ws, .. }) if ws == &workspace_b),
            "expected draft on workspace_b after archiving active thread, got: {:?}",
            sidebar.active_entry,
        );
    });
}

#[gpui::test]
async fn test_unarchive_only_shows_restored_thread(cx: &mut TestAppContext) {
    // Full flow: create a thread, archive it (removing the workspace),
    // then unarchive. Only the restored thread should appear — no
    // leftover drafts or previously-serialized threads.
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    cx.run_until_parked();

    // Create a thread and send a message so it's a real thread.
    let connection = acp_thread::StubAgentConnection::new();
    connection.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
        acp_v1::ContentChunk::new("Hello".into()),
    )]);
    agent_ui::test_support::open_thread_with_connection(&panel, connection, cx);
    agent_ui::test_support::send_message(&panel, cx);
    let session_id = agent_ui::test_support::active_session_id(&panel, cx);
    cx.run_until_parked();

    // Archive it.
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.archive_thread(&session_id, window, cx);
    });
    cx.run_until_parked();

    // Grab metadata for unarchive.
    let thread_id = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entries()
            .find(|e| e.session_id.as_ref() == Some(&session_id))
            .map(|e| e.thread_id)
            .expect("thread should exist")
    });
    let metadata = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry(thread_id)
            .cloned()
            .expect("metadata should exist")
    });

    // Unarchive it — the draft should be replaced by the restored thread.
    let restored_title = metadata.display_title();
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.open_thread_from_archive(metadata, window, cx);
    });
    cx.run_until_parked();

    // The restored thread should be visible. A fresh draft may also be
    // visible as a sidebar row: archive_thread auto-activates one via
    // clear_base_view, and the unarchive then parks it by pushing the
    // restored thread into the base view.
    let entries = visible_entries_as_strings(&sidebar, cx);
    assert!(
        entries.iter().any(|e| e.contains(restored_title.as_ref())),
        "expected the restored thread to be visible, got entries: {entries:?}"
    );
    let thread_count = entries
        .iter()
        .filter(|e| !e.starts_with("v ") && !e.starts_with("> "))
        .count();
    assert!(
        thread_count <= 2,
        "expected at most the restored thread plus a parked draft, got entries: {entries:?}"
    );
}

#[gpui::test]
async fn test_unarchive_first_thread_in_group_does_not_create_spurious_draft(
    cx: &mut TestAppContext,
) {
    // When a thread is unarchived into a project group that has no open
    // workspace, the sidebar opens a new workspace and loads the thread.
    // No spurious draft should appear alongside the unarchived thread.
    agent_ui::test_support::init_test(cx);
    cx.update(|cx| {
        ThreadStore::init_global(cx);
        ThreadMetadataStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
        prompt_store::init(cx);
    });

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/project-a", serde_json::json!({ "src": {} }))
        .await;
    fs.insert_tree("/project-b", serde_json::json!({ "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let project_a = project::Project::test(fs.clone(), ["/project-a".as_ref()], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    cx.run_until_parked();

    // Save an archived thread whose folder_paths point to project-b,
    // which has no open workspace.
    let session_id = acp::SessionId::new(Arc::from("archived-thread"));
    let path_list_b = PathList::new(&[std::path::PathBuf::from("/project-b")]);
    let thread_id = ThreadId::new();
    cx.update(|_, cx| {
        ThreadMetadataStore::global(cx).update(cx, |store, cx| {
            store.save(
                ThreadMetadata {
                    thread_id,
                    session_id: Some(session_id.clone()),
                    agent_id: agent::ZED_AGENT_ID.clone(),
                    title: Some("Unarchived Thread".into()),
                    title_override: None,
                    updated_at: Utc::now(),
                    created_at: None,
                    interacted_at: None,
                    worktree_paths: WorktreePaths::from_folder_paths(&path_list_b),
                    archived: true,
                    remote_connection: None,
                },
                cx,
            )
        });
    });
    cx.run_until_parked();

    // Verify no workspace for project-b exists yet.
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        1,
        "should start with only the project-a workspace"
    );

    // Un-archive the thread — should open project-b workspace and load it.
    let metadata = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry(thread_id)
            .cloned()
            .expect("metadata should exist")
    });

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.open_thread_from_archive(metadata, window, cx);
    });
    cx.run_until_parked();

    // A second workspace should have been created for project-b.
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        2,
        "should have opened a workspace for the unarchived thread"
    );

    // The sidebar should show the unarchived thread without a spurious draft
    // in the project-b group.
    let entries = visible_entries_as_strings(&sidebar, cx);
    let draft_count = entries.iter().filter(|e| e.contains("Draft")).count();
    // project-a gets a draft (it's the active workspace with no threads),
    // but project-b should NOT have one — only the unarchived thread.
    assert!(
        draft_count <= 1,
        "expected at most one draft (for project-a), got entries: {entries:?}"
    );
    assert!(
        entries.iter().any(|e| e.contains("Unarchived Thread")),
        "expected unarchived thread to appear, got entries: {entries:?}"
    );
}

#[gpui::test]
async fn test_unarchive_into_new_workspace_does_not_create_duplicate_real_thread(
    cx: &mut TestAppContext,
) {
    agent_ui::test_support::init_test(cx);
    cx.update(|cx| {
        ThreadStore::init_global(cx);
        ThreadMetadataStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
        prompt_store::init(cx);
    });

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/project-a", serde_json::json!({ "src": {} }))
        .await;
    fs.insert_tree("/project-b", serde_json::json!({ "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let project_a = project::Project::test(fs.clone(), ["/project-a".as_ref()], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    cx.run_until_parked();

    let session_id = acp::SessionId::new(Arc::from("restore-into-new-workspace"));
    let path_list_b = PathList::new(&[PathBuf::from("/project-b")]);
    let original_thread_id = ThreadId::new();
    cx.update(|_, cx| {
        ThreadMetadataStore::global(cx).update(cx, |store, cx| {
            store.save(
                ThreadMetadata {
                    thread_id: original_thread_id,
                    session_id: Some(session_id.clone()),
                    agent_id: agent::ZED_AGENT_ID.clone(),
                    title: Some("Unarchived Thread".into()),
                    title_override: None,
                    updated_at: Utc::now(),
                    created_at: None,
                    interacted_at: None,
                    worktree_paths: WorktreePaths::from_folder_paths(&path_list_b),
                    archived: true,
                    remote_connection: None,
                },
                cx,
            )
        });
    });
    cx.run_until_parked();

    let metadata = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry(original_thread_id)
            .cloned()
            .expect("metadata should exist before unarchive")
    });

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.open_thread_from_archive(metadata, window, cx);
    });

    cx.run_until_parked();

    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        2,
        "expected unarchive to open the target workspace"
    );

    let restored_workspace = multi_workspace.read_with(cx, |mw, cx| {
        mw.workspaces()
            .find(|workspace| PathList::new(&workspace.read(cx).root_paths(cx)) == path_list_b)
            .cloned()
            .expect("expected restored workspace for unarchived thread")
    });
    let restored_panel = restored_workspace.read_with(cx, |workspace, cx| {
        workspace
            .panel::<AgentPanel>(cx)
            .expect("expected unarchive to install an agent panel in the new workspace")
    });

    let restored_thread_id = restored_panel.read_with(cx, |panel, cx| panel.active_thread_id(cx));
    assert_eq!(
        restored_thread_id,
        Some(original_thread_id),
        "expected the new workspace's agent panel to target the restored archived thread id"
    );

    let session_entries = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entries()
            .filter(|entry| entry.session_id.as_ref() == Some(&session_id))
            .cloned()
            .collect::<Vec<_>>()
    });
    assert_eq!(
        session_entries.len(),
        1,
        "expected exactly one metadata row for restored session after opening a new workspace, got: {session_entries:?}"
    );
    assert_eq!(
        session_entries[0].thread_id, original_thread_id,
        "expected restore into a new workspace to reuse the original thread id"
    );
    assert!(
        !session_entries[0].archived,
        "expected restored thread metadata to be unarchived, got: {:?}",
        session_entries[0]
    );

    let mapped_thread_id = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entries()
            .find(|e| e.session_id.as_ref() == Some(&session_id))
            .map(|e| e.thread_id)
    });
    assert_eq!(
        mapped_thread_id,
        Some(original_thread_id),
        "expected session mapping to remain stable after opening the new workspace"
    );

    let entries = visible_entries_as_strings(&sidebar, cx);
    let real_thread_rows = entries
        .iter()
        .filter(|entry| !entry.starts_with("v ") && !entry.starts_with("> "))
        .filter(|entry| !entry.contains("Draft"))
        .count();
    assert_eq!(
        real_thread_rows, 1,
        "expected exactly one visible real thread row after restore into a new workspace, got entries: {entries:?}"
    );
    assert!(
        entries
            .iter()
            .any(|entry| entry.contains("Unarchived Thread")),
        "expected restored thread row to be visible, got entries: {entries:?}"
    );
}

#[gpui::test]
async fn test_unarchive_into_existing_workspace_replaces_draft(cx: &mut TestAppContext) {
    // When a workspace already exists with an empty draft and a thread
    // is unarchived into it, the draft should be replaced — not kept
    // alongside the loaded thread.
    agent_ui::test_support::init_test(cx);
    cx.update(|cx| {
        ThreadStore::init_global(cx);
        ThreadMetadataStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
        prompt_store::init(cx);
    });

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/my-project", serde_json::json!({ "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let project = project::Project::test(fs.clone(), ["/my-project".as_ref()], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    cx.run_until_parked();

    // Create a thread and send a message so it's no longer a draft.
    let connection = acp_thread::StubAgentConnection::new();
    connection.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
        acp_v1::ContentChunk::new("Done".into()),
    )]);
    agent_ui::test_support::open_thread_with_connection(&panel, connection, cx);
    agent_ui::test_support::send_message(&panel, cx);
    let session_id = agent_ui::test_support::active_session_id(&panel, cx);
    cx.run_until_parked();

    // Archive the thread — the group is left empty (no draft created).
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.archive_thread(&session_id, window, cx);
    });
    cx.run_until_parked();

    // Un-archive the thread.
    let thread_id = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entries()
            .find(|e| e.session_id.as_ref() == Some(&session_id))
            .map(|e| e.thread_id)
            .expect("thread should exist in store")
    });
    let metadata = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry(thread_id)
            .cloned()
            .expect("metadata should exist")
    });

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.open_thread_from_archive(metadata, window, cx);
    });
    cx.run_until_parked();

    // The draft should be gone — only the unarchived thread remains.
    let entries = visible_entries_as_strings(&sidebar, cx);
    let draft_count = entries.iter().filter(|e| e.contains("Draft")).count();
    assert_eq!(
        draft_count, 0,
        "expected no drafts after unarchiving, got entries: {entries:?}"
    );
}

#[gpui::test]
async fn test_unarchive_into_inactive_existing_workspace_does_not_leave_active_draft(
    cx: &mut TestAppContext,
) {
    agent_ui::test_support::init_test(cx);
    cx.update(|cx| {
        set_max_idle_retained_threads(1, cx);
        ThreadStore::init_global(cx);
        ThreadMetadataStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
        prompt_store::init(cx);
    });

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/project-a", serde_json::json!({ "src": {} }))
        .await;
    fs.insert_tree("/project-b", serde_json::json!({ "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let project_a = project::Project::test(fs.clone(), ["/project-a".as_ref()], cx).await;
    let project_b = project::Project::test(fs.clone(), ["/project-b".as_ref()], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    let workspace_a = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
    let workspace_b = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(project_b.clone(), window, cx)
    });
    let _panel_b = add_agent_panel(&workspace_b, cx);
    cx.run_until_parked();

    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.activate(workspace_a.clone(), None, window, cx);
    });
    cx.run_until_parked();

    let session_id = acp::SessionId::new(Arc::from("unarchive-into-inactive-existing-workspace"));
    let thread_id = ThreadId::new();
    cx.update(|_, cx| {
        ThreadMetadataStore::global(cx).update(cx, |store, cx| {
            store.save(
                ThreadMetadata {
                    thread_id,
                    session_id: Some(session_id.clone()),
                    agent_id: agent::ZED_AGENT_ID.clone(),
                    title: Some("Restored In Inactive Workspace".into()),
                    title_override: None,
                    updated_at: Utc::now(),
                    created_at: None,
                    interacted_at: None,
                    worktree_paths: WorktreePaths::from_folder_paths(&PathList::new(&[
                        PathBuf::from("/project-b"),
                    ])),
                    archived: true,
                    remote_connection: None,
                },
                cx,
            )
        });
    });
    cx.run_until_parked();

    let metadata = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry(thread_id)
            .cloned()
            .expect("archived metadata should exist before restore")
    });

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.open_thread_from_archive(metadata, window, cx);
    });

    let panel_b_before_settle = workspace_b.read_with(cx, |workspace, cx| {
        workspace.panel::<AgentPanel>(cx).expect(
            "target workspace should still have an agent panel immediately after activation",
        )
    });
    let immediate_active_thread_id =
        panel_b_before_settle.read_with(cx, |panel, cx| panel.active_thread_id(cx));

    cx.run_until_parked();

    sidebar.read_with(cx, |sidebar, _cx| {
        assert_active_thread(
            sidebar,
            &session_id,
            "unarchiving into an inactive existing workspace should end on the restored thread",
        );
    });

    let panel_b = workspace_b.read_with(cx, |workspace, cx| {
        workspace
            .panel::<AgentPanel>(cx)
            .expect("target workspace should still have an agent panel")
    });
    assert_eq!(
        panel_b.read_with(cx, |panel, cx| panel.active_thread_id(cx)),
        Some(thread_id),
        "expected target panel to activate the restored thread id"
    );
    assert!(
        immediate_active_thread_id.is_none() || immediate_active_thread_id == Some(thread_id),
        "expected immediate panel state to be either still loading or already on the restored thread, got active_thread_id={immediate_active_thread_id:?}"
    );

    let entries = visible_entries_as_strings(&sidebar, cx);
    let target_rows: Vec<_> = entries
        .iter()
        .filter(|entry| entry.contains("Restored In Inactive Workspace") || entry.contains("Draft"))
        .cloned()
        .collect();
    assert_eq!(
        target_rows.len(),
        1,
        "expected only the restored row and no surviving draft in the target group, got entries: {entries:?}"
    );
    assert!(
        target_rows[0].contains("Restored In Inactive Workspace"),
        "expected the remaining row to be the restored thread, got entries: {entries:?}"
    );
    assert!(
        !target_rows[0].contains("Draft"),
        "expected no surviving draft row after unarchive into inactive existing workspace, got entries: {entries:?}"
    );
}

#[gpui::test]
async fn test_unarchive_after_removing_parent_project_group_restores_real_thread(
    cx: &mut TestAppContext,
) {
    agent_ui::test_support::init_test(cx);
    cx.update(|cx| {
        set_max_idle_retained_threads(1, cx);
        ThreadStore::init_global(cx);
        ThreadMetadataStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
        prompt_store::init(cx);
    });

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/project-a", serde_json::json!({ "src": {} }))
        .await;
    fs.insert_tree("/project-b", serde_json::json!({ "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let project_a = project::Project::test(fs.clone(), ["/project-a".as_ref()], cx).await;
    let project_b = project::Project::test(fs.clone(), ["/project-b".as_ref()], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    let workspace_b = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(project_b.clone(), window, cx)
    });
    let panel_b = add_agent_panel(&workspace_b, cx);
    cx.run_until_parked();

    let connection = acp_thread::StubAgentConnection::new();
    connection.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
        acp_v1::ContentChunk::new("Done".into()),
    )]);
    agent_ui::test_support::open_thread_with_connection(&panel_b, connection, cx);
    agent_ui::test_support::send_message(&panel_b, cx);
    let session_id = agent_ui::test_support::active_session_id(&panel_b, cx);
    save_test_thread_metadata(&session_id, &project_b, cx).await;
    cx.run_until_parked();

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.archive_thread(&session_id, window, cx);
    });

    cx.run_until_parked();

    let archived_metadata = cx.update(|_, cx| {
        let store = ThreadMetadataStore::global(cx).read(cx);
        let thread_id = store
            .entries()
            .find(|e| e.session_id.as_ref() == Some(&session_id))
            .map(|e| e.thread_id)
            .expect("archived thread should still exist in metadata store");
        let metadata = store
            .entry(thread_id)
            .cloned()
            .expect("archived metadata should still exist after archive");
        assert!(
            metadata.archived,
            "thread should be archived before project removal"
        );
        metadata
    });

    let group_key_b =
        project_b.read_with(cx, |project, cx| ProjectGroupKey::from_project(project, cx));
    let remove_task = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.remove_project_group(&group_key_b, window, cx)
    });
    remove_task
        .await
        .expect("remove project group task should complete");
    cx.run_until_parked();

    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        1,
        "removing the archived thread's parent project group should remove its workspace"
    );

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.open_thread_from_archive(archived_metadata.clone(), window, cx);
    });
    cx.run_until_parked();

    let restored_workspace = multi_workspace.read_with(cx, |mw, cx| {
        mw.workspaces()
            .find(|workspace| {
                PathList::new(&workspace.read(cx).root_paths(cx))
                    == PathList::new(&[PathBuf::from("/project-b")])
            })
            .cloned()
            .expect("expected unarchive to recreate the removed project workspace")
    });
    let restored_panel = restored_workspace.read_with(cx, |workspace, cx| {
        workspace
            .panel::<AgentPanel>(cx)
            .expect("expected restored workspace to bootstrap an agent panel")
    });

    let restored_thread_id = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entries()
            .find(|e| e.session_id.as_ref() == Some(&session_id))
            .map(|e| e.thread_id)
            .expect("session should still map to restored thread id")
    });
    assert_eq!(
        restored_panel.read_with(cx, |panel, cx| panel.active_thread_id(cx)),
        Some(restored_thread_id),
        "expected unarchive after project removal to activate the restored real thread"
    );

    sidebar.read_with(cx, |sidebar, _cx| {
        assert_active_thread(
            sidebar,
            &session_id,
            "expected sidebar active entry to track the restored thread after project removal",
        );
    });

    let entries = visible_entries_as_strings(&sidebar, cx);
    let restored_title = archived_metadata.display_title().to_string();
    let matching_rows: Vec<_> = entries
        .iter()
        .filter(|entry| entry.contains(&restored_title) || entry.contains("Draft"))
        .cloned()
        .collect();
    assert_eq!(
        matching_rows.len(),
        1,
        "expected only one restored row and no surviving draft after unarchive following project removal, got entries: {entries:?}"
    );
    assert!(
        !matching_rows[0].contains("Draft"),
        "expected no draft row after unarchive following project removal, got entries: {entries:?}"
    );
}

#[gpui::test]
async fn test_unarchive_does_not_create_duplicate_real_thread_metadata(cx: &mut TestAppContext) {
    agent_ui::test_support::init_test(cx);
    cx.update(|cx| {
        ThreadStore::init_global(cx);
        ThreadMetadataStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
        prompt_store::init(cx);
    });

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/my-project", serde_json::json!({ "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let project = project::Project::test(fs.clone(), ["/my-project".as_ref()], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    cx.run_until_parked();

    let connection = acp_thread::StubAgentConnection::new();
    connection.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
        acp_v1::ContentChunk::new("Done".into()),
    )]);
    agent_ui::test_support::open_thread_with_connection(&panel, connection, cx);
    agent_ui::test_support::send_message(&panel, cx);
    let session_id = agent_ui::test_support::active_session_id(&panel, cx);
    cx.run_until_parked();

    let original_thread_id = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entries()
            .find(|e| e.session_id.as_ref() == Some(&session_id))
            .map(|e| e.thread_id)
            .expect("thread should exist in store before archiving")
    });

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.archive_thread(&session_id, window, cx);
    });
    cx.run_until_parked();

    let metadata = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry(original_thread_id)
            .cloned()
            .expect("metadata should exist after archiving")
    });

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.open_thread_from_archive(metadata, window, cx);
    });
    cx.run_until_parked();

    let session_entries = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entries()
            .filter(|entry| entry.session_id.as_ref() == Some(&session_id))
            .cloned()
            .collect::<Vec<_>>()
    });

    assert_eq!(
        session_entries.len(),
        1,
        "expected exactly one metadata row for the restored session, got: {session_entries:?}"
    );
    assert_eq!(
        session_entries[0].thread_id, original_thread_id,
        "expected unarchive to reuse the original thread id instead of creating a duplicate row"
    );
    assert!(
        session_entries[0].session_id.is_some(),
        "expected restored metadata to be a real thread, got: {:?}",
        session_entries[0]
    );

    let entries = visible_entries_as_strings(&sidebar, cx);
    let real_thread_rows = entries
        .iter()
        .filter(|entry| !entry.starts_with("v ") && !entry.starts_with("> "))
        .filter(|entry| !entry.contains("Draft"))
        // Parked drafts render with the default title until the user types.
        .filter(|entry| !entry.contains(DEFAULT_THREAD_TITLE))
        .count();
    assert_eq!(
        real_thread_rows, 1,
        "expected exactly one visible real thread row after unarchive, got entries: {entries:?}"
    );
    assert!(
        !entries.iter().any(|entry| entry.contains("Draft")),
        "expected no draft rows after restoring, got entries: {entries:?}"
    );
}

#[gpui::test]
async fn test_switch_to_workspace_with_archived_thread_shows_no_active_entry(
    cx: &mut TestAppContext,
) {
    // When a thread is archived while the user is in a different workspace,
    // clear_base_view creates a draft on the archived workspace's panel.
    // Switching back to that workspace shows the draft as active_entry.
    agent_ui::test_support::init_test(cx);
    cx.update(|cx| {
        ThreadStore::init_global(cx);
        ThreadMetadataStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
        prompt_store::init(cx);
    });

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/project-a", serde_json::json!({ "src": {} }))
        .await;
    fs.insert_tree("/project-b", serde_json::json!({ "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let project_a = project::Project::test(fs.clone(), ["/project-a".as_ref()], cx).await;
    let project_b = project::Project::test(fs.clone(), ["/project-b".as_ref()], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));
    let (sidebar, panel_a) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    let workspace_b = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(project_b.clone(), window, cx)
    });
    let _panel_b = add_agent_panel(&workspace_b, cx);
    cx.run_until_parked();

    // Create a thread in project-a's panel (currently non-active).
    let connection = acp_thread::StubAgentConnection::new();
    connection.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
        acp_v1::ContentChunk::new("Done".into()),
    )]);
    agent_ui::test_support::open_thread_with_connection(&panel_a, connection, cx);
    agent_ui::test_support::send_message(&panel_a, cx);
    let thread_a = agent_ui::test_support::active_session_id(&panel_a, cx);
    cx.run_until_parked();

    // Archive it while project-b is active.
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.archive_thread(&thread_a, window, cx);
    });
    cx.run_until_parked();

    // Switch back to project-a. Its panel was cleared during archiving
    // (clear_base_view activated a draft), so active_entry should point
    // to the draft on workspace_a.
    let workspace_a =
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().next().unwrap().clone());
    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.activate(workspace_a.clone(), None, window, cx);
    });
    cx.run_until_parked();

    sidebar.update_in(cx, |sidebar, _window, cx| {
        sidebar.update_entries(cx);
    });
    cx.run_until_parked();

    sidebar.read_with(cx, |sidebar, _| {
        assert_active_draft(
            sidebar,
            &workspace_a,
            "after switching to workspace with archived thread, active_entry should be the draft",
        );
    });
}

#[gpui::test]
async fn test_archived_threads_excluded_from_sidebar_entries(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_thread_metadata(
        acp::SessionId::new(Arc::from("visible-thread")),
        Some("Visible Thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 2, 0, 0, 0).unwrap(),
        None,
        None,
        &project,
        cx,
    );

    let archived_thread_session_id = acp::SessionId::new(Arc::from("archived-thread"));
    save_thread_metadata(
        archived_thread_session_id.clone(),
        Some("Archived Thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        None,
        None,
        &project,
        cx,
    );

    cx.update(|_, cx| {
        ThreadMetadataStore::global(cx).update(cx, |store, cx| {
            let thread_id = store
                .entries()
                .find(|e| e.session_id.as_ref() == Some(&archived_thread_session_id))
                .map(|e| e.thread_id)
                .unwrap();
            store.archive(thread_id, None, cx)
        })
    });
    cx.run_until_parked();

    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    let entries = visible_entries_as_strings(&sidebar, cx);
    assert!(
        entries.iter().any(|e| e.contains("Visible Thread")),
        "expected visible thread in sidebar, got: {entries:?}"
    );
    assert!(
        !entries.iter().any(|e| e.contains("Archived Thread")),
        "expected archived thread to be hidden from sidebar, got: {entries:?}"
    );

    cx.update(|_, cx| {
        let store = ThreadMetadataStore::global(cx);
        let all: Vec<_> = store.read(cx).entries().collect();
        assert_eq!(
            all.len(),
            2,
            "expected 2 total entries in the store, got: {}",
            all.len()
        );

        let archived: Vec<_> = store.read(cx).archived_entries().collect();
        assert_eq!(archived.len(), 1);
        assert_eq!(
            archived[0].session_id.as_ref().unwrap().0.as_ref(),
            "archived-thread"
        );
    });
}
async fn init_multi_project_test(
    paths: &[&str],
    cx: &mut TestAppContext,
) -> (Arc<FakeFs>, Entity<project::Project>) {
    agent_ui::test_support::init_test(cx);
    cx.update(|cx| {
        set_max_idle_retained_threads(1, cx);
        ThreadStore::init_global(cx);
        ThreadMetadataStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
        prompt_store::init(cx);
    });
    let fs = FakeFs::new(cx.executor());
    for path in paths {
        fs.insert_tree(path, serde_json::json!({ ".git": {}, "src": {} }))
            .await;
    }
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
    let project =
        project::Project::test(fs.clone() as Arc<dyn fs::Fs>, [paths[0].as_ref()], cx).await;
    (fs, project)
}

async fn add_test_project(
    path: &str,
    fs: &Arc<FakeFs>,
    multi_workspace: &Entity<MultiWorkspace>,
    cx: &mut gpui::VisualTestContext,
) -> Entity<Workspace> {
    let project = project::Project::test(fs.clone() as Arc<dyn fs::Fs>, [path.as_ref()], cx).await;
    let workspace = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(project, window, cx)
    });
    cx.run_until_parked();
    workspace
}

#[gpui::test]
async fn test_workspace_lifecycle_retains_projects_when_sidebar_is_closed(cx: &mut TestAppContext) {
    let (fs, project_a) =
        init_multi_project_test(&["/project-a", "/project-b", "/project-c"], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a, window, cx));
    let _sidebar = setup_sidebar_closed(&multi_workspace, cx);

    let workspace_a = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
    assert!(!multi_workspace.read_with(cx, |mw, _| mw.sidebar_open()));
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        1
    );
    assert!(multi_workspace.read_with(cx, |mw, _| mw.workspace() == &workspace_a));

    let workspace_b = add_test_project("/project-b", &fs, &multi_workspace, cx).await;
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        2
    );
    assert!(multi_workspace.read_with(cx, |mw, _| mw.workspace() == &workspace_b));
    assert!(multi_workspace.read_with(cx, |mw, _| mw.workspaces().any(|w| w == &workspace_a)));

    let workspace_c = add_test_project("/project-c", &fs, &multi_workspace, cx).await;
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        3
    );
    assert!(multi_workspace.read_with(cx, |mw, _| mw.workspace() == &workspace_c));
    assert!(multi_workspace.read_with(cx, |mw, _| mw.workspaces().any(|w| w == &workspace_b)));
}

#[gpui::test]
async fn test_workspaces_remain_retained_after_sidebar_closes(cx: &mut TestAppContext) {
    let (fs, project_a) = init_multi_project_test(
        &["/project-a", "/project-b", "/project-c", "/project-d"],
        cx,
    )
    .await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a, window, cx));
    let _sidebar = setup_sidebar(&multi_workspace, cx);
    assert!(multi_workspace.read_with(cx, |mw, _| mw.sidebar_open()));
    let workspace_a = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

    let workspace_b = add_test_project("/project-b", &fs, &multi_workspace, cx).await;
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        2
    );

    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.activate(workspace_a, None, window, cx)
    });
    cx.run_until_parked();
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        2
    );
    assert!(multi_workspace.read_with(cx, |mw, _| mw.workspaces().any(|w| w == &workspace_b)));

    multi_workspace.update_in(cx, |mw, window, cx| mw.close_sidebar(window, cx));
    cx.run_until_parked();
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        2
    );

    let workspace_c = add_test_project("/project-c", &fs, &multi_workspace, cx).await;
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        3
    );
    assert!(multi_workspace.read_with(cx, |mw, _| mw.workspace() == &workspace_c));

    let workspace_d = add_test_project("/project-d", &fs, &multi_workspace, cx).await;
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        4
    );
    assert!(multi_workspace.read_with(cx, |mw, _| mw.workspace() == &workspace_d));
    assert!(multi_workspace.read_with(cx, |mw, _| mw.workspaces().any(|w| w == &workspace_c)));
}

#[gpui::test]
async fn test_sidebar_opening_keeps_existing_retained_workspaces(cx: &mut TestAppContext) {
    let (fs, project_a) =
        init_multi_project_test(&["/project-a", "/project-b", "/project-c"], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a, window, cx));
    setup_sidebar_closed(&multi_workspace, cx);

    let workspace_a = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
    let workspace_b = add_test_project("/project-b", &fs, &multi_workspace, cx).await;
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        2
    );
    assert!(multi_workspace.read_with(cx, |mw, _| mw.workspace() == &workspace_b));
    assert!(multi_workspace.read_with(cx, |mw, _| mw.workspaces().any(|w| w == &workspace_a)));

    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.toggle_sidebar(window, cx);
    });
    cx.run_until_parked();
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        2
    );
    assert!(multi_workspace.read_with(cx, |mw, _| mw.workspaces().any(|w| w == &workspace_b)));

    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.toggle_sidebar(window, cx);
    });

    let workspace_c = add_test_project("/project-c", &fs, &multi_workspace, cx).await;
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        3
    );
    assert!(multi_workspace.read_with(cx, |mw, _| mw.workspace() == &workspace_c));
}
#[gpui::test]
async fn test_startup_failed_restoration_shows_no_draft(cx: &mut TestAppContext) {
    // Empty project groups no longer auto-create drafts via reconciliation.
    // A fresh startup with no restorable thread should show only the header.
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, _panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    let _workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

    let entries = visible_entries_as_strings(&sidebar, cx);
    assert_eq!(
        entries,
        vec!["v [my-project]"],
        "empty group should show only the header, no auto-created draft"
    );
}

#[gpui::test]
async fn test_startup_successful_restoration_no_spurious_draft(cx: &mut TestAppContext) {
    // Rule 5: When the app starts and the AgentPanel successfully loads
    // a thread, no spurious draft should appear.
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    // Create and send a message to make a real thread.
    let connection = StubAgentConnection::new();
    connection.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
        acp_v1::ContentChunk::new("Done".into()),
    )]);
    open_thread_with_connection(&panel, connection, cx);
    send_message(&panel, cx);
    let session_id = active_session_id(&panel, cx);
    save_test_thread_metadata(&session_id, &project, cx).await;
    cx.run_until_parked();

    // Should show the thread, NOT a spurious draft.
    let entries = visible_entries_as_strings(&sidebar, cx);
    assert_eq!(entries, vec!["v [my-project]", "  Hello *"]);

    // active_entry should be Thread, not Draft.
    sidebar.read_with(cx, |sidebar, _| {
        assert_active_thread(sidebar, &session_id, "should be on the thread, not a draft");
    });
}

#[gpui::test]
async fn test_project_header_click_restores_last_viewed(cx: &mut TestAppContext) {
    // Rule 9: Clicking a project header should restore whatever the
    // user was last looking at in that group, not create new drafts
    // or jump to the first entry.
    let project_a = init_test_project_with_agent_panel("/project-a", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));
    let (sidebar, panel_a) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    // Create two threads in project-a.
    let conn1 = StubAgentConnection::new();
    conn1.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
        acp_v1::ContentChunk::new("Done".into()),
    )]);
    open_thread_with_connection(&panel_a, conn1, cx);
    send_message(&panel_a, cx);
    let thread_a1 = active_session_id(&panel_a, cx);
    save_test_thread_metadata(&thread_a1, &project_a, cx).await;

    let conn2 = StubAgentConnection::new();
    conn2.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
        acp_v1::ContentChunk::new("Done".into()),
    )]);
    open_thread_with_connection(&panel_a, conn2, cx);
    send_message(&panel_a, cx);
    let thread_a2 = active_session_id(&panel_a, cx);
    save_test_thread_metadata(&thread_a2, &project_a, cx).await;
    cx.run_until_parked();

    // The user is now looking at thread_a2.
    sidebar.read_with(cx, |sidebar, _| {
        assert_active_thread(sidebar, &thread_a2, "should be on thread_a2");
    });

    // Add project-b and switch to it.
    let fs = cx.update(|_window, cx| <dyn fs::Fs>::global(cx));
    fs.as_fake()
        .insert_tree("/project-b", serde_json::json!({ "src": {} }))
        .await;
    let project_b =
        project::Project::test(fs.clone() as Arc<dyn Fs>, ["/project-b".as_ref()], cx).await;
    let workspace_b = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(project_b.clone(), window, cx)
    });
    let _panel_b = add_agent_panel(&workspace_b, cx);
    cx.run_until_parked();

    // Now switch BACK to project-a by activating its workspace.
    let workspace_a = multi_workspace.read_with(cx, |mw, cx| {
        mw.workspaces()
            .find(|ws| {
                ws.read(cx)
                    .project()
                    .read(cx)
                    .visible_worktrees(cx)
                    .any(|wt| {
                        wt.read(cx)
                            .abs_path()
                            .to_string_lossy()
                            .contains("project-a")
                    })
            })
            .unwrap()
            .clone()
    });
    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.activate(workspace_a.clone(), None, window, cx);
    });
    cx.run_until_parked();

    // The panel should still show thread_a2 (the last thing the user
    // was viewing in project-a), not a draft or thread_a1.
    sidebar.read_with(cx, |sidebar, _| {
        assert_active_thread(
            sidebar,
            &thread_a2,
            "switching back to project-a should restore thread_a2",
        );
    });

    // No spurious draft entries should have been created in
    // project-a's group (project-b may have a placeholder).
    let entries = visible_entries_as_strings(&sidebar, cx);
    // Find project-a's section and check it has no drafts.
    let project_a_start = entries
        .iter()
        .position(|e| e.contains("project-a"))
        .unwrap();
    let project_a_end = entries[project_a_start + 1..]
        .iter()
        .position(|e| e.starts_with("v "))
        .map(|i| i + project_a_start + 1)
        .unwrap_or(entries.len());
    let project_a_drafts = entries[project_a_start..project_a_end]
        .iter()
        .filter(|e| e.contains("Draft"))
        .count();
    assert_eq!(
        project_a_drafts, 0,
        "switching back to project-a should not create drafts in its group"
    );
}

#[gpui::test]
async fn test_activating_workspace_with_draft_does_not_create_extras(cx: &mut TestAppContext) {
    // When a workspace has a draft (from the panel's load fallback)
    // and the user activates it (e.g. by clicking the placeholder or
    // the project header), no extra drafts should be created.
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/project-a", serde_json::json!({ ".git": {}, "src": {} }))
        .await;
    fs.insert_tree("/project-b", serde_json::json!({ ".git": {}, "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let project_a =
        project::Project::test(fs.clone() as Arc<dyn Fs>, ["/project-a".as_ref()], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    let workspace_a = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
    let _panel_a = add_agent_panel(&workspace_a, cx);
    cx.run_until_parked();

    // Add project-b with its own workspace and agent panel.
    let project_b =
        project::Project::test(fs.clone() as Arc<dyn Fs>, ["/project-b".as_ref()], cx).await;
    let workspace_b = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(project_b.clone(), window, cx)
    });
    let _panel_b = add_agent_panel(&workspace_b, cx);
    cx.run_until_parked();

    // Explicitly create a draft on workspace_b so the sidebar tracks one.
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.create_new_thread(&workspace_b, window, cx);
    });
    cx.run_until_parked();

    // Count project-b's drafts.
    let count_b_drafts = |cx: &mut gpui::VisualTestContext| {
        let entries = visible_entries_as_strings(&sidebar, cx);
        entries
            .iter()
            .skip_while(|e| !e.contains("project-b"))
            .take_while(|e| !e.starts_with("v ") || e.contains("project-b"))
            .filter(|e| e.contains("Draft"))
            .count()
    };
    let drafts_before = count_b_drafts(cx);

    // Switch away from project-b, then back.
    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.activate(workspace_a.clone(), None, window, cx);
    });
    cx.run_until_parked();
    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.activate(workspace_b.clone(), None, window, cx);
    });
    cx.run_until_parked();

    let drafts_after = count_b_drafts(cx);
    assert_eq!(
        drafts_before, drafts_after,
        "activating workspace should not create extra drafts"
    );

    // The draft should be highlighted as active after switching back.
    sidebar.read_with(cx, |sidebar, _| {
        assert_active_draft(
            sidebar,
            &workspace_b,
            "draft should be active after switching back to its workspace",
        );
    });
}

#[gpui::test]
async fn test_non_archive_thread_paths_migrate_on_worktree_add_and_remove(cx: &mut TestAppContext) {
    // Historical threads (not open in any agent panel) should have their
    // worktree paths updated when a folder is added to or removed from the
    // project.
    let (_fs, project) = init_multi_project_test(&["/project-a", "/project-b"], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    // Save two threads directly into the metadata store (not via the agent
    // panel), so they are purely historical — no open views hold them.
    // Use different timestamps so sort order is deterministic.
    save_thread_metadata(
        acp::SessionId::new(Arc::from("hist-1")),
        Some("Historical 1".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        None,
        None,
        &project,
        cx,
    );
    save_thread_metadata(
        acp::SessionId::new(Arc::from("hist-2")),
        Some("Historical 2".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 1).unwrap(),
        None,
        None,
        &project,
        cx,
    );
    cx.run_until_parked();
    sidebar.update_in(cx, |sidebar, _window, cx| sidebar.update_entries(cx));
    cx.run_until_parked();

    // Sanity-check: both threads exist under the initial key [/project-a].
    let old_key_paths = PathList::new(&[PathBuf::from("/project-a")]);
    cx.update(|_window, cx| {
        let store = ThreadMetadataStore::global(cx).read(cx);
        assert_eq!(
            store
                .entries_for_main_worktree_path(&old_key_paths, None)
                .count(),
            2,
            "should have 2 historical threads under old key before worktree add"
        );
    });

    // Add a second worktree to the project.
    project
        .update(cx, |project, cx| {
            project.find_or_create_worktree("/project-b", true, cx)
        })
        .await
        .expect("should add worktree");
    cx.run_until_parked();

    // The historical threads should now be indexed under the new combined
    // key [/project-a, /project-b].
    let new_key_paths = PathList::new(&[PathBuf::from("/project-a"), PathBuf::from("/project-b")]);
    cx.update(|_window, cx| {
        let store = ThreadMetadataStore::global(cx).read(cx);
        assert_eq!(
            store
                .entries_for_main_worktree_path(&old_key_paths, None)
                .count(),
            0,
            "should have 0 historical threads under old key after worktree add"
        );
        assert_eq!(
            store
                .entries_for_main_worktree_path(&new_key_paths, None)
                .count(),
            2,
            "should have 2 historical threads under new key after worktree add"
        );
    });

    // Sidebar should show threads under the new header.
    sidebar.update_in(cx, |sidebar, _window, cx| sidebar.update_entries(cx));
    cx.run_until_parked();
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            "v [project-a, project-b]",
            "  Historical 2",
            "  Historical 1",
        ]
    );

    // Now remove the second worktree.
    let worktree_id = project.read_with(cx, |project, cx| {
        project
            .visible_worktrees(cx)
            .find(|wt| wt.read(cx).abs_path().as_ref() == Path::new("/project-b"))
            .map(|wt| wt.read(cx).id())
            .expect("should find project-b worktree")
    });
    project.update(cx, |project, cx| {
        project.remove_worktree(worktree_id, cx);
    });
    cx.run_until_parked();

    // Historical threads should migrate back to the original key.
    cx.update(|_window, cx| {
        let store = ThreadMetadataStore::global(cx).read(cx);
        assert_eq!(
            store
                .entries_for_main_worktree_path(&new_key_paths, None)
                .count(),
            0,
            "should have 0 historical threads under new key after worktree remove"
        );
        assert_eq!(
            store
                .entries_for_main_worktree_path(&old_key_paths, None)
                .count(),
            2,
            "should have 2 historical threads under old key after worktree remove"
        );
    });

    sidebar.update_in(cx, |sidebar, _window, cx| sidebar.update_entries(cx));
    cx.run_until_parked();
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["v [project-a]", "  Historical 2", "  Historical 1",]
    );
}
mod property_test {
    use super::*;
    use gpui::proptest::prelude::*;

    struct TestState {
        fs: Arc<FakeFs>,
        thread_counter: u32,
        workspace_counter: u32,
        saved_thread_ids: Vec<acp::SessionId>,
    }

    impl TestState {
        fn new(fs: Arc<FakeFs>) -> Self {
            Self {
                fs,
                thread_counter: 0,
                workspace_counter: 1,
                saved_thread_ids: Vec::new(),
            }
        }
        fn next_workspace_path(&mut self) -> String {
            let id = self.workspace_counter;
            self.workspace_counter += 1;
            format!("/prop-project-{id}")
        }
    }

    #[derive(Debug)]
    enum Operation {
        SaveThread { project_group_index: usize },
        ToggleAgentPanel,
        CreateDraftThread,
        AddProject,
        ArchiveThread { index: usize },
        SwitchToThread { index: usize },
        SwitchToProjectGroup { index: usize },
        AddWorktreeToProject { project_group_index: usize },
        RemoveWorktreeFromProject { project_group_index: usize },
    }

    // Distribution (out of 24 slots):
    //   SaveThread:                5 slots (~21%)
    //   ToggleAgentPanel:          1 slot  (~4%)
    //   CreateDraftThread:         1 slot  (~4%)
    //   AddProject:                1 slot  (~4%)
    //   ArchiveThread:             2 slots (~8%)
    //   SwitchToThread:            2 slots (~8%)
    //   SwitchToProjectGroup:      2 slots (~8%)
    //   AddWorktreeToProject:      2 slots (~8%)
    //   RemoveWorktreeFromProject: 2 slots (~8%)
    const DISTRIBUTION_SLOTS: u32 = 24;

    impl TestState {
        fn generate_operation(&self, raw: u32, project_group_count: usize) -> Operation {
            let extra = (raw / DISTRIBUTION_SLOTS) as usize;

            match raw % DISTRIBUTION_SLOTS {
                0..=4 => Operation::SaveThread {
                    project_group_index: extra % project_group_count,
                },
                5..=6 => Operation::SaveThread {
                    project_group_index: extra % project_group_count,
                },
                7 => Operation::ToggleAgentPanel,
                8 => Operation::CreateDraftThread,
                9 => Operation::AddProject,
                10..=11 if !self.saved_thread_ids.is_empty() => Operation::ArchiveThread {
                    index: extra % self.saved_thread_ids.len(),
                },
                10..=11 => Operation::AddProject,
                12..=13 if !self.saved_thread_ids.is_empty() => Operation::SwitchToThread {
                    index: extra % self.saved_thread_ids.len(),
                },
                12..=13 => Operation::SwitchToProjectGroup {
                    index: extra % project_group_count,
                },
                14..=15 => Operation::SwitchToProjectGroup {
                    index: extra % project_group_count,
                },
                16..=19 => Operation::SaveThread {
                    project_group_index: extra % project_group_count,
                },
                20..=21 if project_group_count > 0 => Operation::AddWorktreeToProject {
                    project_group_index: extra % project_group_count,
                },
                20..=21 => Operation::SaveThread {
                    project_group_index: extra % project_group_count,
                },
                22..=23 if project_group_count > 0 => Operation::RemoveWorktreeFromProject {
                    project_group_index: extra % project_group_count,
                },
                22..=23 => Operation::SaveThread {
                    project_group_index: extra % project_group_count,
                },
                _ => unreachable!(),
            }
        }
    }
    async fn perform_operation(
        operation: Operation,
        state: &mut TestState,
        multi_workspace: &Entity<MultiWorkspace>,
        sidebar: &Entity<Sidebar>,
        cx: &mut gpui::VisualTestContext,
    ) {
        match operation {
            Operation::SaveThread {
                project_group_index,
            } => {
                // Find a workspace for this project group and create a real
                // thread via its agent panel.
                let (workspace, project) = multi_workspace.read_with(cx, |mw, cx| {
                    let keys = mw.project_group_keys();
                    let key = &keys[project_group_index];
                    let ws = mw
                        .workspaces_for_project_group(key, cx)
                        .next()
                        .cloned()
                        .unwrap_or_else(|| mw.workspace().clone());
                    let project = ws.read(cx).project().clone();
                    (ws, project)
                });

                let panel =
                    workspace.read_with(cx, |workspace, cx| workspace.panel::<AgentPanel>(cx));
                if let Some(panel) = panel {
                    let agent_id = AgentId::new(format!("prop-agent-{}", state.thread_counter));
                    let connection = StubAgentConnection::new().with_agent_id(agent_id.clone());
                    open_thread_with_custom_connection(&panel, connection.clone(), cx);
                    let thread_id = active_thread_id(&panel, cx);
                    let session_id = active_session_id(&panel, cx);
                    // Make the thread non-draft without exercising the prompt
                    // send path; these invariants are about sidebar state.
                    cx.update(|_, cx| {
                        connection.send_update(
                            session_id.clone(),
                            acp_v1::SessionUpdate::AgentMessageChunk(acp_v1::ContentChunk::new(
                                "Done".into(),
                            )),
                            cx,
                        );
                    });
                    cx.run_until_parked();
                    state.saved_thread_ids.push(session_id.clone());

                    let title: SharedString = format!("Thread {}", state.thread_counter).into();
                    state.thread_counter += 1;
                    let updated_at =
                        chrono::TimeZone::with_ymd_and_hms(&chrono::Utc, 2024, 1, 1, 0, 0, 0)
                            .unwrap()
                            + chrono::Duration::seconds(state.thread_counter as i64);
                    let metadata = cx.update(|_, cx| ThreadMetadata {
                        thread_id,
                        session_id: Some(session_id),
                        agent_id,
                        title: Some(title),
                        title_override: None,
                        updated_at,
                        created_at: None,
                        interacted_at: None,
                        worktree_paths: project.read(cx).worktree_paths(cx),
                        archived: false,
                        remote_connection: project.read(cx).remote_connection_options(cx).cloned(),
                    });
                    cx.update(|_, cx| {
                        ThreadMetadataStore::global(cx)
                            .update(cx, |store, cx| store.save(metadata, cx))
                    });
                    cx.run_until_parked();
                }
            }
            Operation::ToggleAgentPanel => {
                let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
                let panel_open =
                    workspace.read_with(cx, |_, cx| AgentPanel::is_visible(&workspace, cx));
                workspace.update_in(cx, |workspace, window, cx| {
                    if panel_open {
                        workspace.close_panel::<AgentPanel>(window, cx);
                    } else {
                        workspace.open_panel::<AgentPanel>(window, cx);
                    }
                });
            }
            Operation::CreateDraftThread => {
                let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
                let panel =
                    workspace.read_with(cx, |workspace, cx| workspace.panel::<AgentPanel>(cx));
                if let Some(panel) = panel {
                    panel.update_in(cx, |panel, window, cx| {
                        panel.new_thread(&NewThread, window, cx);
                    });
                    cx.run_until_parked();
                }
                workspace.update_in(cx, |workspace, window, cx| {
                    workspace.focus_panel::<AgentPanel>(window, cx);
                });
            }
            Operation::AddProject => {
                let path = state.next_workspace_path();
                state
                    .fs
                    .insert_tree(&path, serde_json::json!({ "src": {} }))
                    .await;
                let project = project::Project::test(
                    state.fs.clone() as Arc<dyn fs::Fs>,
                    [path.as_ref()],
                    cx,
                )
                .await;
                project
                    .update(cx, |p, cx| p.wait_for_initial_scan(cx))
                    .await;
                multi_workspace.update_in(cx, |mw, window, cx| {
                    mw.test_add_workspace(project.clone(), window, cx)
                });
            }

            Operation::ArchiveThread { index } => {
                let session_id = state.saved_thread_ids[index].clone();
                sidebar.update_in(cx, |sidebar: &mut Sidebar, window, cx| {
                    sidebar.archive_thread(&session_id, window, cx);
                });
                cx.run_until_parked();
                state.saved_thread_ids.remove(index);
            }
            Operation::SwitchToThread { index } => {
                let session_id = state.saved_thread_ids[index].clone();
                // Find the thread's position in the sidebar entries and select it.
                let thread_index = sidebar.read_with(cx, |sidebar, _| {
                    sidebar.contents.entries.iter().position(|entry| {
                        matches!(
                            entry,
                            ListEntry::Thread(t) if t.metadata.session_id.as_ref() == Some(&session_id)
                        )
                    })
                });
                if let Some(ix) = thread_index {
                    sidebar.update_in(cx, |sidebar, window, cx| {
                        sidebar.selection = Some(ix);
                        sidebar.confirm(&Confirm, window, cx);
                    });
                    cx.run_until_parked();
                }
            }
            Operation::SwitchToProjectGroup { index } => {
                let workspace = multi_workspace.read_with(cx, |mw, cx| {
                    let keys = mw.project_group_keys();
                    let key = &keys[index];
                    mw.workspaces_for_project_group(key, cx)
                        .next()
                        .cloned()
                        .unwrap_or_else(|| mw.workspace().clone())
                });
                multi_workspace.update_in(cx, |mw, window, cx| {
                    mw.activate(workspace, None, window, cx);
                });
            }

            Operation::AddWorktreeToProject {
                project_group_index,
            } => {
                let workspace = multi_workspace.read_with(cx, |mw, cx| {
                    let keys = mw.project_group_keys();
                    let key = &keys[project_group_index];
                    mw.workspaces_for_project_group(key, cx).next().cloned()
                });
                let Some(workspace) = workspace else { return };
                let project = workspace.read_with(cx, |ws, _| ws.project().clone());

                let new_path = state.next_workspace_path();
                state
                    .fs
                    .insert_tree(&new_path, serde_json::json!({ ".git": {}, "src": {} }))
                    .await;

                let result = project
                    .update(cx, |project, cx| {
                        project.find_or_create_worktree(&new_path, true, cx)
                    })
                    .await;
                if result.is_err() {
                    return;
                }
                cx.run_until_parked();
            }
            Operation::RemoveWorktreeFromProject {
                project_group_index,
            } => {
                let workspace = multi_workspace.read_with(cx, |mw, cx| {
                    let keys = mw.project_group_keys();
                    let key = &keys[project_group_index];
                    mw.workspaces_for_project_group(key, cx).next().cloned()
                });
                let Some(workspace) = workspace else { return };
                let project = workspace.read_with(cx, |ws, _| ws.project().clone());

                let worktree_count = project.read_with(cx, |p, cx| p.visible_worktrees(cx).count());
                if worktree_count <= 1 {
                    return;
                }

                let worktree_id = project.read_with(cx, |p, cx| {
                    p.visible_worktrees(cx).last().map(|wt| wt.read(cx).id())
                });
                if let Some(worktree_id) = worktree_id {
                    project.update(cx, |project, cx| {
                        project.remove_worktree(worktree_id, cx);
                    });
                    cx.run_until_parked();
                }
            }
        }
    }

    fn update_sidebar(sidebar: &Entity<Sidebar>, cx: &mut gpui::VisualTestContext) {
        sidebar.update_in(cx, |sidebar, _window, cx| {
            if let Some(mw) = sidebar.multi_workspace.upgrade() {
                mw.update(cx, |mw, _cx| mw.test_expand_all_groups());
            }
            sidebar.update_entries(cx);
        });
    }

    fn validate_sidebar_properties(sidebar: &Sidebar, cx: &App) -> anyhow::Result<()> {
        verify_every_group_in_multiworkspace_is_shown(sidebar, cx)?;
        verify_no_duplicate_threads(sidebar)?;
        verify_all_threads_are_shown(sidebar, cx)?;
        verify_active_state_matches_current_workspace(sidebar, cx)?;
        verify_all_workspaces_are_reachable(sidebar, cx)?;
        verify_workspace_group_key_integrity(sidebar, cx)?;
        Ok(())
    }

    fn verify_no_duplicate_threads(sidebar: &Sidebar) -> anyhow::Result<()> {
        let mut seen: HashSet<acp::SessionId> = HashSet::default();
        let mut duplicates: Vec<(acp::SessionId, String)> = Vec::new();

        for entry in &sidebar.contents.entries {
            if let Some(session_id) = entry.session_id() {
                if !seen.insert(session_id.clone()) {
                    let title = match entry {
                        ListEntry::Thread(thread) => thread.metadata.display_title().to_string(),
                        _ => "<unknown>".to_string(),
                    };
                    duplicates.push((session_id.clone(), title));
                }
            }
        }

        anyhow::ensure!(
            duplicates.is_empty(),
            "threads appear more than once in sidebar: {:?}",
            duplicates,
        );
        Ok(())
    }

    fn verify_every_group_in_multiworkspace_is_shown(
        sidebar: &Sidebar,
        cx: &App,
    ) -> anyhow::Result<()> {
        let Some(multi_workspace) = sidebar.multi_workspace.upgrade() else {
            anyhow::bail!("sidebar should still have an associated multi-workspace");
        };

        let mw = multi_workspace.read(cx);

        // Every project group key in the multi-workspace that has a
        // non-empty path list should appear as a ProjectHeader in the
        // sidebar.
        let all_keys = mw.project_group_keys();
        let expected_keys: HashSet<&ProjectGroupKey> = all_keys
            .iter()
            .filter(|k| !k.path_list().paths().is_empty())
            .collect();

        let sidebar_keys: HashSet<&ProjectGroupKey> = sidebar
            .contents
            .entries
            .iter()
            .filter_map(|entry| match entry {
                ListEntry::ProjectHeader { key, .. } => Some(key),
                _ => None,
            })
            .collect();

        let missing = &expected_keys - &sidebar_keys;
        let stray = &sidebar_keys - &expected_keys;

        anyhow::ensure!(
            missing.is_empty() && stray.is_empty(),
            "sidebar project groups don't match multi-workspace.\n\
             Only in multi-workspace (missing): {:?}\n\
             Only in sidebar (stray): {:?}",
            missing,
            stray,
        );

        Ok(())
    }

    fn verify_all_threads_are_shown(sidebar: &Sidebar, cx: &App) -> anyhow::Result<()> {
        let Some(multi_workspace) = sidebar.multi_workspace.upgrade() else {
            anyhow::bail!("sidebar should still have an associated multi-workspace");
        };
        let workspaces = multi_workspace
            .read(cx)
            .workspaces()
            .cloned()
            .collect::<Vec<_>>();
        let thread_store = ThreadMetadataStore::global(cx);

        let sidebar_thread_ids: HashSet<acp::SessionId> = sidebar
            .contents
            .entries
            .iter()
            .filter_map(|entry| entry.session_id().cloned())
            .collect();

        let mut metadata_thread_ids: HashSet<acp::SessionId> = HashSet::default();

        // Query using the same approach as the sidebar: iterate project
        // group keys, then do main + legacy queries per group.
        let mw = multi_workspace.read(cx);
        let mut workspaces_by_group: HashMap<ProjectGroupKey, Vec<Entity<Workspace>>> =
            HashMap::default();
        for workspace in &workspaces {
            let key = workspace.read(cx).project_group_key(cx);
            workspaces_by_group
                .entry(key)
                .or_default()
                .push(workspace.clone());
        }

        for group_key in mw.project_group_keys() {
            let path_list = group_key.path_list().clone();
            if path_list.paths().is_empty() {
                continue;
            }

            let group_workspaces = workspaces_by_group
                .get(&group_key)
                .map(|ws| ws.as_slice())
                .unwrap_or_default();

            // Main code path queries (run for all groups, even without workspaces).
            // Skip drafts (session_id: None) — they are not shown in the
            // sidebar entries.
            for metadata in thread_store
                .read(cx)
                .entries_for_main_worktree_path(&path_list, None)
            {
                if let Some(sid) = metadata.session_id.clone() {
                    metadata_thread_ids.insert(sid);
                }
            }
            for metadata in thread_store.read(cx).entries_for_path(&path_list, None) {
                if let Some(sid) = metadata.session_id.clone() {
                    metadata_thread_ids.insert(sid);
                }
            }

            // Legacy: per-workspace queries for different root paths.

            for workspace in group_workspaces {
                let ws_path_list = workspace_path_list(workspace, cx);
                if ws_path_list != path_list {
                    for metadata in thread_store.read(cx).entries_for_path(&ws_path_list, None) {
                        if let Some(sid) = metadata.session_id.clone() {
                            metadata_thread_ids.insert(sid);
                        }
                    }
                }
            }
        }

        anyhow::ensure!(
            sidebar_thread_ids == metadata_thread_ids,
            "sidebar threads don't match metadata store: sidebar has {:?}, store has {:?}",
            sidebar_thread_ids,
            metadata_thread_ids,
        );
        Ok(())
    }

    fn verify_active_state_matches_current_workspace(
        sidebar: &Sidebar,
        cx: &App,
    ) -> anyhow::Result<()> {
        let Some(multi_workspace) = sidebar.multi_workspace.upgrade() else {
            anyhow::bail!("sidebar should still have an associated multi-workspace");
        };

        let active_workspace = multi_workspace.read(cx).workspace();

        // 1. active_entry should be Some when the panel has content.
        //    It may be None when the panel is uninitialized (no drafts,
        //    no threads), which is fine.
        //    It may also temporarily point at a different workspace
        //    when the workspace just changed and the new panel has no
        //    content yet.
        let panel = active_workspace.read(cx).panel::<AgentPanel>(cx).unwrap();
        let panel_has_content = panel.read(cx).active_thread_id(cx).is_some()
            || panel.read(cx).active_conversation_view().is_some()
            || panel.read(cx).active_terminal_id().is_some();

        let Some(entry) = sidebar.active_entry.as_ref() else {
            if panel_has_content {
                anyhow::bail!("active_entry is None but panel has content");
            }
            return Ok(());
        };

        // If the entry workspace doesn't match the active workspace
        // and the panel has no content, this is a transient state that
        // will resolve when the panel gets content.
        if entry.workspace().entity_id() != active_workspace.entity_id() && !panel_has_content {
            return Ok(());
        }

        // 2. The entry's workspace must agree with the multi-workspace's
        //    active workspace.
        anyhow::ensure!(
            entry.workspace().entity_id() == active_workspace.entity_id(),
            "active_entry workspace ({:?}) != active workspace ({:?})",
            entry.workspace().entity_id(),
            active_workspace.entity_id(),
        );

        // 3. The entry must match the agent panel's current state.
        if panel.read(cx).active_thread_id(cx).is_some() {
            anyhow::ensure!(
                matches!(entry, ActiveEntry::Thread { .. }),
                "panel shows a tracked draft but active_entry is {:?}",
                entry,
            );
        } else if let Some(thread_id) = panel
            .read(cx)
            .active_conversation_view()
            .map(|cv| cv.read(cx).parent_id())
        {
            anyhow::ensure!(
                matches!(entry, ActiveEntry::Thread { thread_id: tid, .. } if *tid == thread_id),
                "panel has thread {:?} but active_entry is {:?}",
                thread_id,
                entry,
            );
        }

        // 4. Exactly one entry in sidebar contents must be uniquely
        //    identified by the active_entry — unless the panel is showing
        //    the new-draft slot (which is represented by the + button's
        //    active state rather than a sidebar row) or nothing at all.
        // Active terminals must still match a row, so don't treat the absence
        // of a conversation view as "new-draft" when a terminal is active.
        let hidden_from_sidebar = panel.read(cx).active_terminal_id().is_none()
            && (panel.read(cx).active_view_is_new_draft(cx)
                || panel.read(cx).active_conversation_view().is_none());
        if hidden_from_sidebar {
            return Ok(());
        }
        let matching_count = sidebar
            .contents
            .entries
            .iter()
            .filter(|e| entry.matches_entry(e))
            .count();
        if matching_count != 1 {
            let thread_entries: Vec<_> = sidebar
                .contents
                .entries
                .iter()
                .filter_map(|e| match e {
                    ListEntry::Thread(t) => Some(format!(
                        "tid={:?} sid={:?}",
                        t.metadata.thread_id, t.metadata.session_id
                    )),
                    _ => None,
                })
                .collect();
            let store = agent_ui::thread_metadata_store::ThreadMetadataStore::global(cx).read(cx);
            let store_entries: Vec<_> = store
                .entries()
                .map(|m| {
                    format!(
                        "tid={:?} sid={:?} archived={} paths={:?}",
                        m.thread_id,
                        m.session_id,
                        m.archived,
                        m.folder_paths()
                    )
                })
                .collect();
            anyhow::bail!(
                "expected exactly 1 sidebar entry matching active_entry {:?}, found {}. sidebar threads: {:?}. store: {:?}",
                entry,
                matching_count,
                thread_entries,
                store_entries,
            );
        }

        Ok(())
    }

    /// Every workspace in the multi-workspace should be "reachable" from
    /// the sidebar — meaning there is at least one entry (thread, draft,
    /// new-thread, or project header) that, when clicked, would activate
    /// that workspace.
    fn verify_all_workspaces_are_reachable(sidebar: &Sidebar, cx: &App) -> anyhow::Result<()> {
        let Some(multi_workspace) = sidebar.multi_workspace.upgrade() else {
            anyhow::bail!("sidebar should still have an associated multi-workspace");
        };

        let multi_workspace = multi_workspace.read(cx);

        let reachable_workspaces: HashSet<gpui::EntityId> = sidebar
            .contents
            .entries
            .iter()
            .flat_map(|entry| entry.reachable_workspaces(multi_workspace, cx))
            .map(|ws| ws.entity_id())
            .collect();

        let all_workspace_ids: HashSet<gpui::EntityId> = multi_workspace
            .workspaces()
            .map(|ws| ws.entity_id())
            .collect();

        let unreachable = &all_workspace_ids - &reachable_workspaces;

        anyhow::ensure!(
            unreachable.is_empty(),
            "The following workspaces are not reachable from any sidebar entry: {:?}",
            unreachable,
        );

        Ok(())
    }

    fn verify_workspace_group_key_integrity(sidebar: &Sidebar, cx: &App) -> anyhow::Result<()> {
        let Some(multi_workspace) = sidebar.multi_workspace.upgrade() else {
            anyhow::bail!("sidebar should still have an associated multi-workspace");
        };
        multi_workspace
            .read(cx)
            .assert_project_group_key_integrity(cx)
    }

    #[gpui::property_test(config = ProptestConfig {
        cases: 20,
        ..Default::default()
    })]
    async fn test_sidebar_invariants(
        #[strategy = gpui::proptest::collection::vec(0u32..DISTRIBUTION_SLOTS * 10, 1..10)]
        raw_operations: Vec<u32>,
        cx: &mut TestAppContext,
    ) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT_PROPTEST_DB: AtomicUsize = AtomicUsize::new(0);

        let test_db_id = NEXT_PROPTEST_DB.fetch_add(1, Ordering::SeqCst);
        cx.update(|cx| {
            cx.set_global(TestTerminalMetadataDbName(format!(
                "PROPTEST_TERMINAL_THREAD_METADATA_{test_db_id}"
            )));
        });

        agent_ui::test_support::init_test(cx);
        cx.update(|cx| {
            cx.set_global(db::AppDatabase::test_new());
            set_max_idle_retained_threads(1, cx);
            cx.set_global(agent_ui::thread_metadata_store::TestMetadataDbName(
                format!("PROPTEST_THREAD_METADATA_{test_db_id}"),
            ));

            ThreadStore::init_global(cx);
            ThreadMetadataStore::init_global(cx);
            language_model::LanguageModelRegistry::test(cx);
            prompt_store::init(cx);

            // Auto-add an AgentPanel to every workspace so that implicitly
            // created workspaces (e.g. from thread activation) also have one.
            cx.observe_new(
                |workspace: &mut Workspace,
                 window: Option<&mut Window>,
                 cx: &mut gpui::Context<Workspace>| {
                    if let Some(window) = window {
                        let panel = cx.new(|cx| AgentPanel::test_new(workspace, window, cx));
                        workspace.add_panel(panel, window, cx);
                    }
                },
            )
            .detach();
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/my-project",
            serde_json::json!({
                ".git": {},
                "src": {},
            }),
        )
        .await;
        cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
        let project =
            project::Project::test(fs.clone() as Arc<dyn fs::Fs>, ["/my-project".as_ref()], cx)
                .await;
        project
            .update(cx, |p, cx| p.wait_for_initial_scan(cx))
            .await;

        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let sidebar = setup_sidebar(&multi_workspace, cx);

        let mut state = TestState::new(fs);
        let mut executed: Vec<String> = Vec::new();

        for &raw_op in &raw_operations {
            let project_group_count =
                multi_workspace.read_with(cx, |mw, _| mw.project_group_keys().len());
            let operation = state.generate_operation(raw_op, project_group_count);
            executed.push(format!("{:?}", operation));
            perform_operation(operation, &mut state, &multi_workspace, &sidebar, cx).await;
            cx.run_until_parked();

            update_sidebar(&sidebar, cx);
            cx.run_until_parked();

            let result =
                sidebar.read_with(cx, |sidebar, cx| validate_sidebar_properties(sidebar, cx));
            if let Err(err) = result {
                let log = executed.join("\n  ");
                panic!(
                    "Property violation after step {}:\n{err}\n\nOperations:\n  {log}",
                    executed.len(),
                );
            }
        }
    }
}
#[test]
fn test_worktree_info_branch_names_for_main_worktrees() {
    let folder_paths = PathList::new(&[PathBuf::from("/projects/myapp")]);
    let worktree_paths = WorktreePaths::from_folder_paths(&folder_paths);

    let branch_by_path: HashMap<PathBuf, SharedString> =
        [(PathBuf::from("/projects/myapp"), "feature-x".into())]
            .into_iter()
            .collect();

    let infos = worktree_info_from_thread_paths(&worktree_paths, &branch_by_path);
    assert_eq!(infos.len(), 1);
    assert_eq!(infos[0].kind, ui::WorktreeKind::Main);
    assert_eq!(infos[0].branch_name, Some(SharedString::from("feature-x")));
    assert_eq!(infos[0].worktree_name, Some(SharedString::from("myapp")));
}

#[test]
fn test_worktree_info_branch_names_for_linked_worktrees() {
    let main_paths = PathList::new(&[PathBuf::from("/projects/myapp")]);
    let folder_paths = PathList::new(&[PathBuf::from("/projects/myapp-feature")]);
    let worktree_paths =
        WorktreePaths::from_path_lists(main_paths, folder_paths).expect("same length");

    let branch_by_path: HashMap<PathBuf, SharedString> = [(
        PathBuf::from("/projects/myapp-feature"),
        "feature-branch".into(),
    )]
    .into_iter()
    .collect();

    let infos = worktree_info_from_thread_paths(&worktree_paths, &branch_by_path);
    assert_eq!(infos.len(), 1);
    assert_eq!(infos[0].kind, ui::WorktreeKind::Linked);
    assert_eq!(
        infos[0].branch_name,
        Some(SharedString::from("feature-branch"))
    );
}

#[test]
fn test_worktree_info_missing_branch_returns_none() {
    let folder_paths = PathList::new(&[PathBuf::from("/projects/myapp")]);
    let worktree_paths = WorktreePaths::from_folder_paths(&folder_paths);

    let branch_by_path: HashMap<PathBuf, SharedString> = HashMap::new();

    let infos = worktree_info_from_thread_paths(&worktree_paths, &branch_by_path);
    assert_eq!(infos.len(), 1);
    assert_eq!(infos[0].kind, ui::WorktreeKind::Main);
    assert_eq!(infos[0].branch_name, None);
    assert_eq!(infos[0].worktree_name, Some(SharedString::from("myapp")));
}
#[gpui::test]
async fn test_remote_archive_thread_with_disconnected_remote(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    // When a remote thread has no linked-worktree state to archive (only
    // a main worktree), archival is a pure metadata operation: no RPCs
    // are issued against the remote server. This must succeed even when
    // the connection has dropped out, because losing connectivity should
    // not block users from cleaning up their thread list.
    //
    // Threads that *do* have linked-worktree state require a live
    // connection to run the git worktree removal on the server; that
    // path is covered by `test_remote_archive_thread_with_active_connection`.
    init_test(cx);

    cx.update(|cx| {
        release_channel::init(semver::Version::new(0, 0, 0), cx);
    });

    let app_state = cx.update(|cx| {
        let app_state = workspace::AppState::test(cx);
        workspace::init(app_state.clone(), cx);
        app_state
    });

    server_cx.update(|cx| {
        release_channel::init(semver::Version::new(0, 0, 0), cx);
    });

    let server_fs = FakeFs::new(server_cx.executor());
    server_fs
        .insert_tree(
            "/project",
            serde_json::json!({
                ".git": {},
                "src": { "main.rs": "fn main() {}" },
            }),
        )
        .await;

    let (project, _headless, _opts) = start_remote_project(
        &server_fs,
        Path::new("/project"),
        &app_state,
        None,
        cx,
        server_cx,
    )
    .await;
    let remote_client = project
        .read_with(cx, |project, _cx| project.remote_client())
        .expect("remote project should expose its client");

    cx.update(|cx| <dyn fs::Fs>::set_global(app_state.fs.clone(), cx));

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    let thread_id = acp::SessionId::new(Arc::from("remote-thread"));
    save_thread_metadata(
        thread_id.clone(),
        Some("Remote Thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&chrono::Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        None,
        None,
        &project,
        cx,
    );
    cx.run_until_parked();

    // Sanity-check: there is nothing on the remote fs outside the main
    // repo, so archival should not need to touch the server.
    assert!(
        !server_fs.is_dir(Path::new("/worktrees")).await,
        "no linked worktrees on the server before archiving"
    );

    // Disconnect the remote connection before archiving. We don't
    // `run_until_parked` here because the disconnect itself triggers
    // reconnection work that can't complete in the test environment.
    remote_client.update(cx, |client, cx| {
        client.simulate_disconnect(cx).detach();
    });

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.archive_thread(&thread_id, window, cx);
    });
    cx.run_until_parked();

    let is_archived = cx.update(|_window, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry_by_session(&thread_id)
            .map(|t| t.archived)
            .unwrap_or(false)
    });
    assert!(
        is_archived,
        "thread should be archived even when remote is disconnected"
    );

    let entries = visible_entries_as_strings(&sidebar, cx);
    assert!(
        !entries.iter().any(|e| e.contains("Remote Thread")),
        "archived thread should be hidden from sidebar: {entries:?}"
    );
}

#[gpui::test]
async fn test_collab_guest_move_thread_paths_is_noop(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/project-a", serde_json::json!({ "src": {} }))
        .await;
    fs.insert_tree("/project-b", serde_json::json!({ "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
    let project = project::Project::test(fs, ["/project-a".as_ref()], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));

    // Set up the sidebar while the project is local. This registers the
    // WorktreePathsChanged subscription for the project.
    let _sidebar = setup_sidebar(&multi_workspace, cx);

    let session_id = acp::SessionId::new(Arc::from("test-thread"));
    save_named_thread_metadata("test-thread", "My Thread", &project, cx).await;

    let thread_id = cx.update(|_window, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry_by_session(&session_id)
            .map(|e| e.thread_id)
            .expect("thread must be in the store")
    });

    cx.update(|_window, cx| {
        let store = ThreadMetadataStore::global(cx);
        let entry = store.read(cx).entry(thread_id).unwrap();
        assert_eq!(
            entry.folder_paths().paths(),
            &[PathBuf::from("/project-a")],
            "thread must be saved with /project-a before collab"
        );
    });

    // Transition the project into collab mode. The sidebar's subscription is
    // still active from when the project was local.
    project.update(cx, |project, _cx| {
        project.mark_as_collab_for_testing();
    });

    // Adding a worktree fires WorktreePathsChanged with old_paths = {/project-a}.
    // The sidebar's subscription is still active, so move_thread_paths is called.
    // Without the is_via_collab() guard inside move_thread_paths, this would
    // update the stored thread paths from {/project-a} to {/project-a, /project-b}.
    project
        .update(cx, |project, cx| {
            project.find_or_create_worktree("/project-b", true, cx)
        })
        .await
        .expect("should add worktree");
    cx.run_until_parked();

    cx.update(|_window, cx| {
        let store = ThreadMetadataStore::global(cx);
        let entry = store
            .read(cx)
            .entry(thread_id)
            .expect("thread must still exist");
        assert_eq!(
            entry.folder_paths().paths(),
            &[PathBuf::from("/project-a")],
            "thread path must not change when project is via collab"
        );
    });
}
#[test]
fn test_split_leading_icon_char() {
    // A leading symbol set off by whitespace is pulled out and trimmed from the
    // title.
    let (icon, title, positions) =
        split_leading_icon_char(&"✳ Implement separate config".into(), &[]).unwrap();
    assert_eq!(icon.as_ref(), "✳");
    assert_eq!(title.as_ref(), "Implement separate config");
    assert_eq!(positions, Vec::<usize>::new());

    // No prefix when the title starts with a letter.
    assert!(split_leading_icon_char(&"Implement separate config".into(), &[]).is_none());

    // Leading whitespace is not treated as a prefix.
    assert!(split_leading_icon_char(&" leading space".into(), &[]).is_none());

    // An alphanumeric prefix such as a version marker is not treated as an icon.
    assert!(split_leading_icon_char(&"v1 Running".into(), &[]).is_none());
    assert!(split_leading_icon_char(&"1 first".into(), &[]).is_none());

    // A title consisting only of a symbol (no whitespace separator) is left
    // untouched.
    assert!(split_leading_icon_char(&"✳".into(), &[]).is_none());
    assert!(split_leading_icon_char(&"✳Thinking".into(), &[]).is_none());

    // A run of the same symbol collapses to a single glyph.
    let (icon, title, _) = split_leading_icon_char(&">>> Thinking".into(), &[]).unwrap();
    assert_eq!(icon.as_ref(), ">");
    assert_eq!(title.as_ref(), "Thinking");

    // Surrounding ASCII brackets are stripped so the inner glyph is used.
    let (icon, title, _) = split_leading_icon_char(&"[!] codex waiting".into(), &[]).unwrap();
    assert_eq!(icon.as_ref(), "!");
    assert_eq!(title.as_ref(), "codex waiting");

    // A run of dots is condensed into an ellipsis.
    let (icon, title, _) = split_leading_icon_char(&"... working".into(), &[]).unwrap();
    assert_eq!(icon.as_ref(), "\u{2026}");
    assert_eq!(title.as_ref(), "working");

    let (icon, title, _) = split_leading_icon_char(&"[...] working".into(), &[]).unwrap();
    assert_eq!(icon.as_ref(), "\u{2026}");
    assert_eq!(title.as_ref(), "working");

    let (icon, title, _) = split_leading_icon_char(&"[…] working".into(), &[]).unwrap();
    assert_eq!(icon.as_ref(), "\u{2026}");
    assert_eq!(title.as_ref(), "working");

    // Multi-codepoint emoji are kept intact rather than sliced mid-cluster.
    let (icon, title, _) = split_leading_icon_char(&"🇺🇸 flag".into(), &[]).unwrap();
    assert_eq!(icon.as_ref(), "🇺🇸");
    assert_eq!(title.as_ref(), "flag");

    // Highlight positions are shifted to account for the stripped prefix, and
    // positions that fall inside the stripped prefix are dropped.
    let title: SharedString = "# abc".into();
    let abc_offset = title.find('a').unwrap();
    let (icon, trimmed, positions) =
        split_leading_icon_char(&title, &[0, abc_offset, abc_offset + 1]).unwrap();
    assert_eq!(icon.as_ref(), "#");
    assert_eq!(trimmed.as_ref(), "abc");
    assert_eq!(positions, vec![0, 1]);
}

#[gpui::test]
async fn test_find_or_create_workspace_returns_the_created_remote_workspace(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let local_project = init_test_project("/local", cx).await;
    cx.update(|cx| {
        release_channel::init(semver::Version::new(0, 0, 0), cx);
    });
    server_cx.update(|cx| {
        release_channel::init(semver::Version::new(0, 0, 0), cx);
    });
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(local_project, window, cx));
    let local_workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

    let server_fs = FakeFs::new(server_cx.executor());
    server_fs
        .insert_tree("/remote-project", serde_json::json!({ "src": {} }))
        .await;
    let (opts, server_session, _) = remote::RemoteClient::fake_server(cx, server_cx);
    server_cx.update(remote_server::HeadlessProject::init);
    let server_executor = server_cx.executor();
    let _headless = server_cx.new(|cx| {
        remote_server::HeadlessProject::new(
            remote_server::HeadlessAppState {
                session: server_session,
                fs: server_fs.clone(),
                http_client: Arc::new(http_client::BlockedHttpClient),
                node_runtime: node_runtime::NodeRuntime::unavailable(),
                languages: Arc::new(language::LanguageRegistry::new(server_executor)),
                extension_host_proxy: Arc::new(extension::ExtensionHostProxy::new()),
                startup_time: std::time::Instant::now(),
            },
            false,
            cx,
        )
    });
    let remote_client = remote::RemoteClient::connect_mock(opts.clone(), cx).await;

    // Stand in for the save prompt from a concurrent workspace removal: as
    // soon as the remote workspace is activated mid-open, activate the local
    // workspace again. The open must still return the workspace it created,
    // not whichever workspace is active once it finishes.
    multi_workspace.update_in(cx, |_, window, cx| {
        let local_workspace = local_workspace.clone();
        cx.subscribe_in(&cx.entity(), window, move |this, _, event, window, cx| {
            if matches!(event, MultiWorkspaceEvent::WorkspaceAdded(_)) {
                this.activate(local_workspace.clone(), None, window, cx);
            }
        })
        .detach();
    });

    let created = multi_workspace
        .update_in(cx, |mw, window, cx| {
            let key = ProjectGroupKey::new(
                Some(opts.clone()),
                PathList::new(&[PathBuf::from("/remote-project")]),
            );
            mw.find_or_create_workspace(
                PathList::new(&[PathBuf::from("/remote-project")]),
                Some(opts),
                Some(key),
                move |_, _, _| Task::ready(Ok(Some(remote_client))),
                None,
                workspace::OpenMode::Activate,
                None,
                window,
                cx,
            )
        })
        .await
        .expect("opening the remote project should succeed");
    cx.run_until_parked();

    assert_eq!(
        created.read_with(cx, |workspace, cx| PathList::new(&workspace.root_paths(cx))),
        PathList::new(&[PathBuf::from("/remote-project")]),
        "the returned workspace should be the remote workspace that was created"
    );
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspace().clone()),
        local_workspace,
        "the local workspace should have re-activated during the open"
    );
}

fn set_sidebar_test_panel_alpha(alpha: f32, cx: &mut VisualTestContext) {
    cx.update(|window, cx| {
        let mut theme = cx.theme().as_ref().clone();
        theme.styles.colors.background = Hsla::from(gpui::rgb(0xdcdcdd));
        theme.styles.colors.surface_background = Hsla::from(gpui::rgb(0xff00ff));
        theme.styles.colors.panel_background = Hsla::from(gpui::rgb(0xebebec)).alpha(alpha);
        theme.styles.colors.element_background = Hsla::from(gpui::rgb(0xebebec));
        theme.styles.colors.ghost_element_hover = Hsla::from(gpui::rgb(0xdfdfe0));
        theme::GlobalTheme::update_theme(cx, Arc::new(theme));
        window.refresh();
    });
    cx.run_until_parked();
}

fn assert_sidebar_action_hover(
    selector: &'static str,
    row_bounds: Bounds<Pixels>,
    cx: &mut VisualTestContext,
) {
    cx.simulate_mouse_move(row_bounds.center(), None, Modifiers::default());
    let button_bounds = cx.debug_bounds(selector).expect("visible row action");
    assert_eq!(
        row_bounds.intersect(&button_bounds),
        button_bounds,
        "{selector}: action must belong to the row"
    );
    cx.simulate_mouse_move(button_bounds.center(), None, Modifiers::default());
    let background = sidebar_painted_background_at(button_bounds.center(), cx);
    assert_ne!(
        background,
        sidebar_painted_background_at(row_bounds.center(), cx),
        "{selector}: hovered action must contrast with the row"
    );
}

fn sidebar_painted_background_at(position: Point<Pixels>, cx: &mut VisualTestContext) -> u32 {
    cx.update(|window, _| {
        let position = position.scale(window.scale_factor());
        let color = window
            .painted_quads()
            .into_iter()
            .filter(|quad| {
                !quad.background.is_transparent()
                    && quad
                        .bounds
                        .intersect(&quad.content_mask.bounds)
                        .contains(&position)
            })
            .max_by_key(|quad| quad.order)
            .expect("painted background at pointer")
            .background
            .as_solid()
            .expect("solid background at pointer");
        u32::from(Rgba::from(color))
    })
}
