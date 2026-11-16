use super::*;
use fs::FakeFs;
use gpui::TestAppContext;
use language::{LanguageConfig, LanguageMatcher};
use serde_json::json;
use util::rel_path::rel_path;

fn cpp_language() -> Arc<Language> {
    Arc::new(Language::new(
        LanguageConfig {
            name: "C++".into(),
            matcher: LanguageMatcher {
                path_suffixes: vec!["cpp".into(), "hpp".into()],
                ..Default::default()
            }
            .into(),
            ..Default::default()
        },
        Some(tree_sitter_cpp::LANGUAGE.into()),
    ))
}

async fn setup(cx: &mut TestAppContext) -> (Arc<FakeFs>, Entity<Project>, Entity<Buffer>) {
    cx.update(|cx| {
        let settings = settings::SettingsStore::test(cx);
        cx.set_global(settings);
        release_channel::init(semver::Version::new(0, 0, 0), cx);
    });
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/symbols", json!({
        "main.cpp": "#include \"widget.hpp\"\nvoid run(Widget widget) { widget.reset(); } // comment",
        "widget.hpp": "struct Widget { void reset(); int field; };"
    })).await;
    let project = Project::test(fs.clone(), [std::path::Path::new("/symbols")], cx).await;
    project.read_with(cx, |project, _| project.languages().add(cpp_language()));
    let buffer = project
        .update(cx, |project, cx| {
            project.open_local_buffer("/symbols/main.cpp", cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();
    (fs, project, buffer)
}

#[gpui::test]
async fn syntax_index_is_demand_driven_and_rejects_invalid_positions(cx: &mut TestAppContext) {
    let (fs, project, buffer) = setup(cx).await;
    fs.insert_file(
        std::path::Path::new("/symbols/other.hpp"),
        "struct Other {};".into(),
    )
    .await;
    cx.run_until_parked();
    project.read_with(cx, |project, cx| {
        assert!(
            project
                .lsp_store
                .read(cx)
                .symbol_index
                .cache
                .lock()
                .entries
                .is_empty()
        );
    });
    let offset = buffer.read_with(cx, |buffer, _| buffer.text().find("comment").unwrap());
    assert!(
        project
            .update(cx, |project, cx| project.definitions(&buffer, offset, cx))
            .await
            .unwrap()
            .is_none()
    );
    project.read_with(cx, |project, cx| {
        let store = &project.lsp_store.read(cx).symbol_index;
        assert!(store.jobs.lock().pending.is_empty());
        assert_eq!(store.cache.lock().entries.len(), 1);
    });
}

#[gpui::test(iterations = 10)]
async fn syntax_index_shares_scans_and_releases_cancelled_jobs(cx: &mut TestAppContext) {
    let (_, project, buffer) = setup(cx).await;
    let (first, second) = project.update(cx, |project, cx| {
        project.lsp_store.update(cx, |store, cx| {
            (
                store.prepare_symbol_index(&buffer, Some(SymbolQueryTrace::new("definition")), cx),
                store.prepare_symbol_index(&buffer, Some(SymbolQueryTrace::new("references")), cx),
            )
        })
    });
    let (first, second) = futures::join!(first, second);
    let first = first.unwrap();
    let second = second.unwrap();
    assert!(Arc::ptr_eq(&first, &second));
    assert_eq!(first.files.len(), 2);
    drop((first, second));
    let task = project.update(cx, |project, cx| {
        project.lsp_store.update(cx, |store, cx| {
            store.prepare_symbol_index(&buffer, None, cx)
        })
    });
    drop(task);
    cx.run_until_parked();
    project.read_with(cx, |project, cx| {
        let jobs = project.lsp_store.read(cx).symbol_index.jobs.lock();
        assert!(
            jobs.pending
                .values()
                .all(|job| job.task.upgrade().is_none() && job.ready.upgrade().is_none())
        );
    });
}

#[test]
fn syntax_index_cache_retains_working_set_when_scan_exceeds_budget() {
    let language = cpp_language();
    let document = index_text("struct Widget { void reset(); };", &language, None).unwrap();
    let path = |name| ProjectPath {
        worktree_id: worktree::WorktreeId::from_usize(1),
        path: rel_path(name).into(),
    };
    let cost = document.memory_cost();
    let mut cache = DocumentCache {
        budget: cost * 2,
        ..Default::default()
    };
    cache.insert(path("one.hpp"), "one".into(), document.clone(), false);
    cache.insert(path("two.hpp"), "two".into(), document.clone(), false);
    cache.insert(path("three.hpp"), "three".into(), document.clone(), false);
    assert_eq!(cache.bytes, cost * 2);
    assert!(cache.get(&path("one.hpp"), "one").is_some());
    assert!(cache.get(&path("two.hpp"), "two").is_some());
    assert_eq!(cache.admission_skipped, 1);
    cache.insert(path("hot.cpp"), "hot".into(), document, true);
    assert_eq!(cache.entries.len(), 2);
    assert_eq!(cache.evictions, 1);
    assert!(cache.get(&path("hot.cpp"), "hot").is_some());
    assert!(cache.get(&path("hot.cpp"), "changed").is_none());
    assert_eq!(cache.bytes, cost);
}

#[gpui::test]
async fn syntax_index_stale_close_does_not_crash_or_close_other_peer(cx: &mut TestAppContext) {
    let (_, project, buffer) = setup(cx).await;
    let buffers = project.read_with(cx, |project, _| project.buffer_store().clone());
    let first = proto::PeerId::default();
    let second = proto::PeerId { owner_id: 1, id: 1 };
    buffers
        .update(cx, |store, cx| {
            store.create_buffer_for_peer(&buffer, first, cx)
        })
        .await
        .unwrap();
    buffers
        .update(cx, |store, cx| {
            store.create_buffer_for_peer(&buffer, second, cx)
        })
        .await
        .unwrap();
    let buffer_id = buffer.read_with(cx, |buffer, _| buffer.remote_id().to_proto());
    for peer in [first, first] {
        crate::buffer_store::BufferStore::handle_close_buffer(
            buffers.clone(),
            TypedEnvelope {
                sender_id: peer,
                original_sender_id: None,
                message_id: 1,
                payload: proto::CloseBuffer {
                    project_id: 0,
                    buffer_id,
                },
                received_at: Instant::now(),
            },
            cx.to_async(),
        )
        .await
        .unwrap();
        assert!(buffers.read_with(cx, |store, _| store.has_shared_buffers()));
    }
    buffers.update(cx, |store, _| store.forget_shared_buffers());
    crate::buffer_store::BufferStore::handle_close_buffer(
        buffers.clone(),
        TypedEnvelope {
            sender_id: second,
            original_sender_id: None,
            message_id: 2,
            payload: proto::CloseBuffer {
                project_id: 0,
                buffer_id,
            },
            received_at: Instant::now(),
        },
        cx.to_async(),
    )
    .await
    .unwrap();
    assert!(!buffers.read_with(cx, |store, _| store.has_shared_buffers()));
}

#[gpui::test]
async fn syntax_index_cancel_releases_server_request(cx: &mut TestAppContext) {
    let (_, project, buffer) = setup(cx).await;
    let store = project.read_with(cx, |project, _| project.lsp_store.clone());
    let payload = buffer.read_with(cx, |buffer, _| proto::GetSyntaxSymbols {
        project_id: 0,
        buffer_id: buffer.remote_id().to_proto(),
        position: Some(language::proto::serialize_anchor(&buffer.anchor_before(0))),
        version: vec![proto::VectorClockEntry {
            replica_id: 42,
            timestamp: 1,
        }],
        kind: proto::get_syntax_symbols::Kind::Definition.into(),
        trace_id: "cancel-test".into(),
    });
    let request_store = store.clone();
    let request = cx.spawn(async move |cx| {
        LspStore::handle_get_syntax_symbols(
            request_store,
            TypedEnvelope {
                sender_id: proto::PeerId::default(),
                original_sender_id: None,
                message_id: 1,
                payload,
                received_at: Instant::now(),
            },
            cx.clone(),
        )
        .await
    });
    cx.run_until_parked();
    assert_eq!(
        store.read_with(cx, |store, _| store
            .symbol_index
            .remote_queries
            .lock()
            .active
            .len()),
        1
    );
    LspStore::handle_cancel_syntax_symbols(
        store.clone(),
        TypedEnvelope {
            sender_id: proto::PeerId::default(),
            original_sender_id: None,
            message_id: 2,
            payload: proto::CancelSyntaxSymbols {
                project_id: 0,
                trace_id: "cancel-test".into(),
            },
            received_at: Instant::now(),
        },
        cx.to_async(),
    )
    .await
    .unwrap();
    assert!(request.await.is_err());
    assert!(store.read_with(cx, |store, _| {
        store.symbol_index.remote_queries.lock().active.is_empty()
    }));
}

#[gpui::test]
async fn syntax_index_cancellation_before_request_prevents_scan(cx: &mut TestAppContext) {
    let (_, project, buffer) = setup(cx).await;
    let store = project.read_with(cx, |project, _| project.lsp_store.clone());
    LspStore::handle_cancel_syntax_symbols(
        store.clone(),
        TypedEnvelope {
            sender_id: proto::PeerId::default(),
            original_sender_id: None,
            message_id: 1,
            payload: proto::CancelSyntaxSymbols {
                project_id: 0,
                trace_id: "early-cancel".into(),
            },
            received_at: Instant::now(),
        },
        cx.to_async(),
    )
    .await
    .unwrap();
    let payload = buffer.read_with(cx, |buffer, _| proto::GetSyntaxSymbols {
        project_id: 0,
        buffer_id: buffer.remote_id().to_proto(),
        position: Some(language::proto::serialize_anchor(&buffer.anchor_before(0))),
        version: language::proto::serialize_version(&buffer.version()),
        kind: proto::get_syntax_symbols::Kind::Definition.into(),
        trace_id: "early-cancel".into(),
    });
    assert!(
        LspStore::handle_get_syntax_symbols(
            store.clone(),
            TypedEnvelope {
                sender_id: proto::PeerId::default(),
                original_sender_id: None,
                message_id: 2,
                payload,
                received_at: Instant::now(),
            },
            cx.to_async()
        )
        .await
        .is_err()
    );
    store.read_with(cx, |store, _| {
        assert!(store.symbol_index.remote_queries.lock().active.is_empty());
        assert!(
            store
                .symbol_index
                .remote_queries
                .lock()
                .cancelled
                .is_empty()
        );
        assert!(store.symbol_index.jobs.lock().pending.is_empty());
        assert!(store.symbol_index.cache.lock().entries.is_empty());
    });
}
