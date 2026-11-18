use super::*;
use fs::FakeFs;
use gpui::{TestAppContext, UpdateGlobal as _};
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
async fn syntax_index_finishes_disk_scan_after_queries_are_cancelled(cx: &mut TestAppContext) {
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

    let (build_lock, previous) = project.read_with(cx, |project, cx| {
        let store = &project.lsp_store.read(cx).symbol_index;
        let previous = store.jobs.lock().disk.clone().unwrap();
        (store.build_lock.clone(), previous)
    });
    let guard = build_lock.lock().await;
    let task = project.update(cx, |project, cx| {
        project.lsp_store.update(cx, |store, cx| {
            store.symbol_index.jobs.lock().generation += 1;
            store.prepare_symbol_index(&buffer, None, cx)
        })
    });
    cx.run_until_parked();
    drop(task);
    cx.run_until_parked();
    drop(guard);
    cx.run_until_parked();
    project.read_with(cx, |project, cx| {
        let jobs = project.lsp_store.read(cx).symbol_index.jobs.lock();
        let current = jobs.disk.as_ref().unwrap();
        assert!(!Arc::ptr_eq(current, &previous));
        assert_eq!(current.generation.worktree, jobs.generation);
        assert_eq!(current.entries.len(), previous.entries.len());
        assert!(
            jobs.pending
                .values()
                .all(|job| job.task.upgrade().is_none() && job.ready.upgrade().is_none())
        );
    });
}

#[test]
fn syntax_index_buffer_cache_evicts_least_recently_used_document() {
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
    cache.insert(path("one.hpp"), "one".into(), document.clone());
    cache.insert(path("two.hpp"), "two".into(), document.clone());
    for name in ["one.hpp", "two.hpp"] {
        cache.entries.get_mut(&path(name)).unwrap().accessed_at -= Duration::from_secs(1);
    }
    assert!(cache.get(&path("one.hpp"), "one").is_some());
    cache.insert(path("hot.cpp"), "hot".into(), document);
    assert_eq!(cache.bytes, cost * 2);
    assert!(cache.get(&path("one.hpp"), "one").is_some());
    assert!(cache.get(&path("two.hpp"), "two").is_none());
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
    for peer in [first, second] {
        buffers
            .update(cx, |store, cx| {
                store.create_buffer_for_peer(&buffer, peer, cx)
            })
            .await
            .unwrap();
    }
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
    let third = proto::PeerId { owner_id: 1, id: 2 };
    buffers
        .update(cx, |store, cx| {
            store.create_buffer_for_peer(&buffer, third, cx)
        })
        .await
        .unwrap();
    buffers.update(cx, |store, _| store.forget_shared_buffers_for(&second));
    assert!(buffers.read_with(cx, |store, _| store.has_shared_buffers()));
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
    assert!(buffers.read_with(cx, |store, _| store.has_shared_buffers()));
    buffers.update(cx, |store, _| store.forget_shared_buffers_for(&third));
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

#[gpui::test]
async fn syntax_index_reuses_disk_documents_and_refreshes_changed_files(cx: &mut TestAppContext) {
    let (fs, project, buffer) = setup(cx).await;
    let query = |cx: &mut TestAppContext| {
        project.update(cx, |project, cx| {
            let offset = buffer.read(cx).text().find("reset").unwrap();
            project.definitions(&buffer, offset, cx)
        })
    };
    assert!(query(cx).await.unwrap().is_some());
    let disk = |cx: &mut TestAppContext| {
        project.read_with(cx, |project, cx| {
            project
                .lsp_store
                .read(cx)
                .symbol_index
                .jobs
                .lock()
                .disk
                .clone()
                .unwrap()
        })
    };
    let first = disk(cx);
    cx.update(|cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store.set_user_settings(r#"{"tab_size": 8}"#, cx).unwrap();
        });
    });
    buffer.update(cx, |buffer, cx| buffer.edit([(0..0, " ")], None, cx));
    assert!(query(cx).await.unwrap().is_some());
    assert!(Arc::ptr_eq(&first, &disk(cx)));

    fs.insert_file(
        std::path::Path::new("/symbols/widget.hpp"),
        "struct Widget { void refresh(); };".into(),
    )
    .await;
    cx.run_until_parked();
    assert!(query(cx).await.unwrap().is_none());
    let next = disk(cx);
    assert!(!Arc::ptr_eq(&first, &next));
    let before = first
        .entries
        .iter()
        .find(|(path, _)| path.path.as_unix_str() == "main.cpp")
        .unwrap();
    let after = next.entries.get(before.0).unwrap();
    assert!(Arc::ptr_eq(
        before.1.document.as_ref().unwrap(),
        after.document.as_ref().unwrap()
    ));
    cx.update(|cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store
                .set_user_settings(r#"{"modeline_lines": 0}"#, cx)
                .unwrap();
        });
    });
    assert!(query(cx).await.unwrap().is_none());
    assert_ne!(next.generation.settings, disk(cx).generation.settings);
}

#[gpui::test]
async fn syntax_index_rejects_an_anchor_from_another_buffer(cx: &mut TestAppContext) {
    let (_, project, buffer) = setup(cx).await;
    let other = project
        .update(cx, |project, cx| {
            project.open_local_buffer("/symbols/widget.hpp", cx)
        })
        .await
        .unwrap();
    let store = project.read_with(cx, |project, _| project.lsp_store.clone());
    let request = proto::GetSyntaxSymbols {
        project_id: 0,
        buffer_id: buffer.read_with(cx, |buffer, _| buffer.remote_id().into()),
        position: Some(other.read_with(cx, |buffer, _| {
            language::proto::serialize_anchor(&buffer.anchor_before(1))
        })),
        version: buffer.read_with(cx, |buffer, _| {
            language::proto::serialize_version(&buffer.version())
        }),
        kind: proto::get_syntax_symbols::Kind::Definition.into(),
        trace_id: "wrong-buffer".into(),
    };
    let error = LspStore::handle_get_syntax_symbols(
        store,
        TypedEnvelope {
            sender_id: proto::PeerId::default(),
            original_sender_id: None,
            message_id: 1,
            payload: request,
            received_at: Instant::now(),
        },
        cx.to_async(),
    )
    .await
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("does not resolve in the source buffer")
    );
}

#[gpui::test]
async fn syntax_index_dropping_remote_query_sends_cancel(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    #[derive(Default)]
    struct PendingQuery {
        request_id: Option<String>,
        finish: Option<futures::channel::oneshot::Sender<()>>,
        cancelled: bool,
    }

    for context in [&mut *cx, &mut *server_cx] {
        context.update(|cx| release_channel::init(semver::Version::new(0, 0, 0), cx));
    }
    let (options, server, guard) = remote::RemoteClient::fake_server(cx, server_cx);
    let pending_query = server_cx.new(|_| PendingQuery::default());
    server.add_request_handler(
        pending_query.downgrade(),
        |_, _: TypedEnvelope<proto::Ping>, _| async { Ok(proto::Ack {}) },
    );
    server.add_entity_request_handler(
        |query: Entity<PendingQuery>,
         request: TypedEnvelope<proto::GetSyntaxSymbols>,
         mut cx| async move {
            let (finish, finished) = futures::channel::oneshot::channel();
            query.update(&mut cx, |query, _| {
                query.request_id = Some(request.payload.trace_id);
                query.finish = Some(finish);
            });
            finished.await?;
            anyhow::bail!("Cancelled test query");
        },
    );
    server.add_entity_message_handler(
        |query: Entity<PendingQuery>,
         cancel: TypedEnvelope<proto::CancelSyntaxSymbols>,
         mut cx| async move {
            query.update(&mut cx, |query, _| {
                assert_eq!(query.request_id.as_deref(), Some(cancel.payload.trace_id.as_str()));
                query.cancelled = true;
                query.finish.take().unwrap().send(()).unwrap();
            });
            Ok(())
        },
    );
    server.subscribe_to_entity(0, &pending_query);
    drop(guard);

    let remote = remote::RemoteClient::connect_mock(options, cx).await;
    let client = remote.read_with(cx, |remote, _| remote.proto_client());
    assert!(client.supports_syntax_symbols());
    let trace = SymbolQueryTrace::new("definition");
    let request = proto::GetSyntaxSymbols {
        trace_id: trace.0.id.clone(),
        ..Default::default()
    };
    let task = cx.update(|cx| send_remote_symbol_query(client, request, trace, cx));
    cx.run_until_parked();
    assert!(pending_query.read_with(server_cx, |query, _| query.finish.is_some()));

    drop(task);
    cx.run_until_parked();
    assert!(pending_query.read_with(server_cx, |query, _| query.cancelled));
}

#[gpui::test]
async fn syntax_index_retries_files_after_a_failed_read(cx: &mut TestAppContext) {
    let (fs, project, _) = setup(cx).await;
    let header = std::path::Path::new("/symbols/marked.data");
    let source = b"// -*- mode: C++; -*-\nstruct Marked {};";
    fs.insert_file(header, source.to_vec()).await;
    cx.run_until_parked();
    let (languages, worktrees) = project.read_with(cx, |project, cx| {
        let worktrees: Vec<_> = project
            .worktree_store()
            .read(cx)
            .worktrees()
            .map(|worktree| {
                let worktree = worktree.read(cx);
                (worktree.snapshot(), worktree.full_path(RelPath::empty()))
            })
            .collect();
        (project.languages().clone(), worktrees)
    });
    let generation = DiskGeneration {
        worktree: 0,
        settings: 0,
        languages: languages.language_configuration_version(),
    };
    let worktree_ids: Vec<_> = worktrees
        .iter()
        .map(|(worktree, _)| worktree.id())
        .collect();
    let settings = cx.update(|cx| DiskLanguageSettings::new(cx, &worktree_ids));
    let mut disk = build_disk_index(
        fs.as_ref(),
        &languages,
        &settings,
        &worktrees,
        None,
        generation,
        None,
    )
    .await;
    let path = ProjectPath {
        worktree_id: worktree_ids[0],
        path: rel_path("marked.data").into(),
    };
    let entry = disk.entries.get_mut(&path).unwrap();
    assert_eq!(
        entry.document.as_ref().unwrap().language,
        SymbolLanguage::Cpp
    );
    entry.modified = None;
    fs::Fs::remove_file(fs.as_ref(), header, fs::RemoveOptions::default())
        .await
        .unwrap();

    for _ in 0..2 {
        disk = build_disk_index(
            fs.as_ref(),
            &languages,
            &settings,
            &worktrees,
            Some(&disk),
            generation,
            None,
        )
        .await;
        assert!(disk.retry_after.is_some());
        assert_eq!(disk.failed_files.get(&path), Some(&SymbolLanguage::Cpp));
        assert!(!disk.entries.contains_key(&path));
    }

    fs.insert_file(header, source.to_vec()).await;
    let recovered = build_disk_index(
        fs.as_ref(),
        &languages,
        &settings,
        &worktrees,
        Some(&disk),
        generation,
        None,
    )
    .await;
    assert!(recovered.retry_after.is_none());
    assert!(recovered.failed_files.is_empty());
    assert!(recovered.entries.get(&path).unwrap().document.is_some());
}

#[gpui::test]
async fn syntax_index_keeps_clean_overlays_and_avoids_navigation_from_a_truncated_index(
    cx: &mut TestAppContext,
) {
    let (_, project, buffer) = setup(cx).await;
    let store = project.read_with(cx, |project, _| project.lsp_store.clone());
    let header = project
        .update(cx, |project, cx| {
            project.open_local_buffer("/symbols/widget.hpp", cx)
        })
        .await
        .unwrap();
    let prepared = store
        .update(cx, |store, cx| {
            store.prepare_symbol_index(&buffer, None, cx)
        })
        .await
        .unwrap();
    let header_document = prepared
        .files
        .iter()
        .find(|file| {
            file.path
                .as_ref()
                .is_some_and(|path| path.path.as_unix_str() == "widget.hpp")
        })
        .unwrap();
    store.read_with(cx, |store, _| {
        let mut jobs = store.symbol_index.jobs.lock();
        let disk = jobs.disk.as_ref().unwrap();
        let cached = disk
            .entries
            .get(header_document.path.as_ref().unwrap())
            .unwrap();
        assert!(Arc::ptr_eq(
            cached.document.as_ref().unwrap(),
            &header_document.document
        ));
        jobs.disk = Some(Arc::new(DiskIndex {
            generation: disk.generation,
            entries: disk.entries.clone(),
            limit_reached: true,
            retry_after: None,
            failed_files: HashMap::default(),
            failed_grammars: HashSet::default(),
        }));
    });
    drop(prepared);

    let source = "#include \"widget.hpp\"\nvoid run(Widget widget) { int local = 0; ++local; widget.reset(); }";
    buffer.update(cx, |buffer, cx| buffer.set_text(source, cx));
    let definitions = |name: &str, cx: &mut TestAppContext| {
        let offset = source.rfind(name).unwrap();
        project.update(cx, |project, cx| project.definitions(&buffer, offset, cx))
    };
    assert!(definitions("reset", cx).await.unwrap().is_none());
    let local = definitions("local", cx).await.unwrap().unwrap();
    assert_eq!(local.len(), 1);
    assert_eq!(local[0].target.buffer, buffer);
    drop(header);
}

#[gpui::test]
async fn syntax_index_survives_lazy_load_and_caches_unavailable_grammars(cx: &mut TestAppContext) {
    let (_, project, buffer) = setup(cx).await;
    let languages = project.read_with(cx, |project, _| project.languages().clone());
    languages.register_test_language(LanguageConfig {
        name: "Java".into(),
        ..Default::default()
    });
    let loader_ready = Arc::new(AtomicBool::new(false));
    let kotlin = LanguageConfig {
        name: "Kotlin".into(),
        grammar: Some("test-grammar".into()),
        ..Default::default()
    };
    languages.register_native_grammars([("test-grammar", tree_sitter_cpp::LANGUAGE)]);
    languages.register_language(
        kotlin.name.clone(),
        kotlin.grammar.clone(),
        kotlin.matcher.clone(),
        false,
        None,
        Arc::new({
            let loader_ready = loader_ready.clone();
            move || {
                let config = kotlin.clone();
                let ready = loader_ready.load(Ordering::Acquire);
                async move {
                    anyhow::ensure!(ready, "test language not readable yet");
                    Ok(language::LoadedLanguage {
                        config,
                        queries: Default::default(),
                        context_provider: None,
                        toolchain_provider: None,
                        manifest_name: None,
                    })
                }
                .boxed()
            }
        }),
    );
    let version = languages.version();
    let configuration_version = languages.language_configuration_version();
    let store = project.read_with(cx, |project, _| project.lsp_store.clone());
    let prepare = |cx: &mut TestAppContext| {
        store.update(cx, |store, cx| {
            store.prepare_symbol_index(&buffer, None, cx)
        })
    };
    let first = prepare(cx).await.unwrap();
    assert!(first.complete);
    assert!(languages.version() > version);
    assert_eq!(
        languages.language_configuration_version(),
        configuration_version
    );

    let first_disk = store.read_with(cx, |store, _| {
        store.symbol_index.jobs.lock().disk.clone().unwrap()
    });
    assert!(first_disk.failed_grammars.contains(&SymbolLanguage::Kotlin));
    drop(first);
    let second = prepare(cx).await.unwrap();
    assert!(second.complete);
    store.read_with(cx, |store, _| {
        let jobs = store.symbol_index.jobs.lock();
        assert!(Arc::ptr_eq(jobs.disk.as_ref().unwrap(), &first_disk));
    });
    drop(second);

    loader_ready.store(true, Ordering::Release);
    assert!(
        languages
            .language_for_name("Kotlin")
            .await
            .unwrap()
            .grammar()
            .is_some()
    );
    assert_eq!(
        languages.language_configuration_version(),
        configuration_version
    );
    assert!(prepare(cx).await.unwrap().complete);
    store.read_with(cx, |store, _| {
        let jobs = store.symbol_index.jobs.lock();
        let disk = jobs.disk.as_ref().unwrap();
        assert!(!Arc::ptr_eq(disk, &first_disk));
        assert!(disk.failed_grammars.is_empty());
    });
}

#[gpui::test]
async fn syntax_index_rejects_a_scan_after_relevant_settings_change(cx: &mut TestAppContext) {
    let (_, project, buffer) = setup(cx).await;
    let store = project.read_with(cx, |project, _| project.lsp_store.clone());
    let build_lock = store.read_with(cx, |store, _| store.symbol_index.build_lock.clone());
    let guard = build_lock.lock().await;
    let task = store.update(cx, |store, cx| {
        store.prepare_symbol_index(&buffer, None, cx)
    });
    cx.update(|cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store
                .set_user_settings(r#"{"modeline_lines": 0}"#, cx)
                .unwrap();
        });
    });
    cx.run_until_parked();
    drop(guard);
    assert!(task.await.is_err());
    store.read_with(cx, |store, _| {
        assert!(store.symbol_index.jobs.lock().disk.is_none());
    });
}
