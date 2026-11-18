use super::*;
use fs::Fs as _;
use pretty_assertions::assert_eq;

fn cpp_language() -> Arc<Language> {
    Arc::new(Language::new(
        LanguageConfig {
            name: "C++".into(),
            matcher: LanguageMatcher {
                path_suffixes: vec!["cpp".into(), "hpp".into()],
                modeline_aliases: vec!["cpp".into()],
                ..Default::default()
            }
            .into(),
            ..Default::default()
        },
        Some(tree_sitter_cpp::LANGUAGE.into()),
    ))
}

async fn setup(cx: &mut TestAppContext) -> (Arc<FakeFs>, Entity<Project>, Entity<Buffer>) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/symbols"),
        json!({
            "widget.hpp": "struct Widget { void reset(); int field; };",
            "widget.cpp": "#include \"widget.hpp\"\nvoid Widget::reset() {}",
            "main.cpp": "#include \"widget.hpp\"\nvoid run(Widget widget) { widget.reset(); }",
            ".gitignore": "ignored.hpp\n",
            "ignored.hpp": "struct Ignored {};"
        }),
    )
    .await;
    let project = Project::test(fs.clone(), [path!("/symbols").as_ref()], cx).await;
    project
        .read_with(cx, |project, _| project.languages().clone())
        .add(cpp_language());
    let buffer = project
        .update(cx, |project, cx| {
            project.open_local_buffer(path!("/symbols/main.cpp"), cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();
    (fs, project, buffer)
}

#[gpui::test]
async fn syntax_symbols_navigation_references_and_completions(cx: &mut TestAppContext) {
    let (_, project, buffer) = setup(cx).await;
    let offset = buffer.read_with(cx, |buffer, _| buffer.text().rfind("reset").unwrap());
    let definitions = project
        .update(cx, |project, cx| project.definitions(&buffer, offset, cx))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(definitions.len(), 1);
    definitions[0].target.buffer.read_with(cx, |buffer, _| {
        assert!(buffer.text().contains("Widget::reset"))
    });
    let declarations = project
        .update(cx, |project, cx| project.declarations(&buffer, offset, cx))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(declarations.len(), 1);
    declarations[0].target.buffer.read_with(cx, |buffer, _| {
        assert!(buffer.text().contains("struct Widget"))
    });

    let references = project
        .update(cx, |project, cx| project.references(&buffer, offset, cx))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(references.len(), 3);
    assert!(references.iter().any(|location| location.buffer == buffer));
    let completions = project
        .update(cx, |project, cx| {
            project.completions(&buffer, offset + 2, DEFAULT_COMPLETION_CONTEXT, cx)
        })
        .await
        .unwrap();
    let completions = &completions[0].completions;
    assert!(
        completions
            .iter()
            .any(|completion| completion.new_text == "reset"
                && completion.kind() == Some(lsp::CompletionItemKind::METHOD))
    );
    assert!(
        completions
            .iter()
            .all(|completion| completion.source.server_id().is_none())
    );
    let replacement = completions[0]
        .replace_range
        .to_offset(&buffer.read_with(cx, |buffer, _| buffer.snapshot()));
    assert_eq!(replacement, offset..offset + "reset".len());
}

#[gpui::test]
async fn syntax_symbols_unsaved_overlay_and_disk_changes(cx: &mut TestAppContext) {
    let (fs, project, buffer) = setup(cx).await;
    let header = project
        .update(cx, |project, cx| {
            project.open_local_buffer(path!("/symbols/widget.hpp"), cx)
        })
        .await
        .unwrap();
    header.update(cx, |buffer, cx| {
        buffer.set_text("struct Widget { void refresh(); };", cx)
    });
    buffer.update(cx, |buffer, cx| {
        buffer.set_text("void run(Widget widget) { widget.refresh(); }", cx)
    });
    let offset = buffer.read_with(cx, |buffer, _| buffer.text().find("refresh").unwrap());
    let definitions = project
        .update(cx, |project, cx| project.definitions(&buffer, offset, cx))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(definitions.len(), 1);
    assert_eq!(definitions[0].target.buffer, header);

    fs.insert_file(path!("/symbols/new.hpp"), "struct FreshType {};".into())
        .await;
    cx.run_until_parked();
    buffer.update(cx, |buffer, cx| {
        buffer.set_text("void run() { FreshType value; }", cx)
    });
    let offset = buffer.read_with(cx, |buffer, _| buffer.text().find("FreshType").unwrap());
    assert!(
        project
            .update(cx, |project, cx| project.definitions(&buffer, offset, cx))
            .await
            .unwrap()
            .is_some()
    );
}

#[gpui::test]
async fn syntax_symbols_ignored_files_and_comments(cx: &mut TestAppContext) {
    let (_, project, buffer) = setup(cx).await;
    buffer.update(cx, |buffer, cx| {
        buffer.set_text("void run() { Ignored value; } // Wid", cx)
    });
    let ignored = buffer.read_with(cx, |buffer, _| buffer.text().find("Ignored").unwrap());
    assert!(
        project
            .update(cx, |project, cx| project.definitions(&buffer, ignored, cx))
            .await
            .unwrap()
            .is_none()
    );
    let end = buffer.read_with(cx, |buffer, _| buffer.len());
    assert!(
        project
            .update(cx, |project, cx| project.completions(
                &buffer,
                end,
                DEFAULT_COMPLETION_CONTEXT,
                cx
            ))
            .await
            .unwrap()
            .is_empty()
    );
}

#[gpui::test]
async fn syntax_symbols_preserve_lsp_results_and_fall_back_on_empty(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    let text = "struct Widget { void reset(); }; void run(Widget widget) { widget.reset(); }";
    fs.insert_tree(path!("/symbols"), json!({ "main.cpp": text }))
        .await;
    let project = Project::test(fs, [path!("/symbols").as_ref()], cx).await;
    let languages = project.read_with(cx, |project, _| project.languages().clone());
    languages.add(cpp_language());
    let mut servers = languages.register_fake_lsp(
        "C++",
        FakeLspAdapter {
            capabilities: lsp::ServerCapabilities {
                definition_provider: Some(lsp::OneOf::Left(true)),
                references_provider: Some(lsp::OneOf::Left(true)),
                completion_provider: Some(lsp::CompletionOptions::default()),
                ..Default::default()
            },
            ..Default::default()
        },
    );
    let (buffer, _handle) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/symbols/main.cpp"), cx)
        })
        .await
        .unwrap();
    let server = servers.next().await.unwrap();
    cx.run_until_parked();
    server.set_request_handler::<lsp::request::GotoDefinition, _, _>(|_, _| async {
        Ok(Some(lsp::GotoDefinitionResponse::Scalar(
            lsp::Location::new(
                lsp::Uri::from_file_path(path!("/symbols/main.cpp")).unwrap(),
                lsp::Range::new(lsp::Position::new(0, 0), lsp::Position::new(0, 6)),
            ),
        )))
    });
    server.set_request_handler::<lsp::request::References, _, _>(|_, _| async {
        Ok(Some(vec![lsp::Location::new(
            lsp::Uri::from_file_path(path!("/symbols/main.cpp")).unwrap(),
            lsp::Range::new(lsp::Position::new(0, 0), lsp::Position::new(0, 6)),
        )]))
    });
    server.set_request_handler::<lsp::request::Completion, _, _>(|_, _| async {
        Ok(Some(lsp::CompletionResponse::Array(vec![
            lsp::CompletionItem {
                label: "from_lsp".into(),
                ..Default::default()
            },
        ])))
    });
    let offset = text.rfind("reset").unwrap();
    let definitions = project
        .update(cx, |project, cx| project.definitions(&buffer, offset, cx))
        .await
        .unwrap()
        .unwrap();
    let snapshot = buffer.read_with(cx, |buffer, _| buffer.snapshot());
    assert_eq!(definitions.len(), 1);
    assert_eq!(definitions[0].target.range.to_offset(&snapshot), 0..6);
    let references = project
        .update(cx, |project, cx| project.references(&buffer, offset, cx))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(references.len(), 1);
    assert_eq!(references[0].range.to_offset(&snapshot), 0..6);
    let completions = project
        .update(cx, |project, cx| {
            project.completions(&buffer, offset + 2, DEFAULT_COMPLETION_CONTEXT, cx)
        })
        .await
        .unwrap();
    assert_eq!(completions.len(), 1);
    assert_eq!(completions[0].completions.len(), 1);
    assert_eq!(completions[0].completions[0].new_text, "from_lsp");

    server.set_request_handler::<lsp::request::GotoDefinition, _, _>(|_, _| async { Ok(None) });
    server.set_request_handler::<lsp::request::References, _, _>(|_, _| async { Ok(None) });
    server.set_request_handler::<lsp::request::Completion, _, _>(|_, _| async { Ok(None) });
    let definitions = project
        .update(cx, |project, cx| project.definitions(&buffer, offset, cx))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(definitions.len(), 1);
    assert_eq!(
        definitions[0].target.range.to_offset(&snapshot),
        text.find("reset").unwrap()..text.find("reset").unwrap() + 5
    );
    assert!(
        project
            .update(cx, |project, cx| project.references(&buffer, offset, cx))
            .await
            .unwrap()
            .is_some()
    );
    let completions = project
        .update(cx, |project, cx| {
            project.completions(&buffer, offset + 2, DEFAULT_COMPLETION_CONTEXT, cx)
        })
        .await
        .unwrap();
    assert!(
        completions
            .iter()
            .flat_map(|response| &response.completions)
            .any(|completion| completion.new_text == "reset"
                && completion.source.server_id().is_none())
    );
}

#[gpui::test]
async fn syntax_symbols_invalidate_closed_files_on_change_rename_and_delete(
    cx: &mut TestAppContext,
) {
    let (fs, project, buffer) = setup(cx).await;
    fs.insert_file(path!("/symbols/fresh.hpp"), "struct FreshType {};".into())
        .await;
    cx.run_until_parked();
    buffer.update(cx, |buffer, cx| buffer.set_text("Fresh", cx));
    let completions = project
        .update(cx, |project, cx| {
            project.completions(&buffer, 5, DEFAULT_COMPLETION_CONTEXT, cx)
        })
        .await
        .unwrap();
    assert!(
        completions
            .iter()
            .flat_map(|response| &response.completions)
            .any(|completion| completion.new_text == "FreshType")
    );

    fs.insert_file(path!("/symbols/fresh.hpp"), "struct FreshNext {};".into())
        .await;
    cx.run_until_parked();
    let completions = project
        .update(cx, |project, cx| {
            project.completions(&buffer, 5, DEFAULT_COMPLETION_CONTEXT, cx)
        })
        .await
        .unwrap();
    assert!(
        completions
            .iter()
            .flat_map(|response| &response.completions)
            .any(|completion| completion.new_text == "FreshNext")
    );
    assert!(
        !completions
            .iter()
            .flat_map(|response| &response.completions)
            .any(|completion| completion.new_text == "FreshType")
    );

    fs.rename(
        path!("/symbols/fresh.hpp").as_ref(),
        path!("/symbols/renamed.hpp").as_ref(),
        Default::default(),
    )
    .await
    .unwrap();
    cx.run_until_parked();
    let completions = project
        .update(cx, |project, cx| {
            project.completions(&buffer, 5, DEFAULT_COMPLETION_CONTEXT, cx)
        })
        .await
        .unwrap();
    assert!(
        completions
            .iter()
            .flat_map(|response| &response.completions)
            .any(|completion| completion.new_text == "FreshNext")
    );

    buffer.update(cx, |buffer, cx| buffer.set_text("FreshNext value;", cx));
    let locations = project
        .update(cx, |project, cx| project.definitions(&buffer, 3, cx))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(locations.len(), 1);
    locations[0].target.buffer.read_with(cx, |buffer, _| {
        assert_eq!(buffer.file().unwrap().path().as_unix_str(), "renamed.hpp");
    });
    drop(locations);
    buffer.update(cx, |buffer, cx| buffer.set_text("Fresh", cx));

    fs.remove_file(path!("/symbols/renamed.hpp").as_ref(), Default::default())
        .await
        .unwrap();
    cx.run_until_parked();
    assert!(
        project
            .update(cx, |project, cx| project.completions(
                &buffer,
                5,
                DEFAULT_COMPLETION_CONTEXT,
                cx
            ))
            .await
            .unwrap()
            .is_empty()
    );
}

#[gpui::test]
async fn syntax_symbols_normalize_crlf_and_decode_bom_files(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    let text = "#include \"crlf.hpp\"\n#include \"bom.hpp\"\n#include \"utf16.hpp\"\nvoid run(Crlf one, Bom two, Utf16 three) { one.reset(); two.reset(); three.reset(); }";
    fs.insert_tree(path!("/symbols"), json!({ "main.cpp": text }))
        .await;
    for (name, class, encoding, bom) in [
        ("crlf.hpp", "Crlf", encoding_rs::UTF_8, false),
        ("bom.hpp", "Bom", encoding_rs::UTF_8, true),
        ("utf16.hpp", "Utf16", encoding_rs::UTF_16LE, true),
    ] {
        let text = format!("// 😀\r\nstruct {class} {{\r\n    void reset();\r\n}};\r\n");
        fs.insert_file(
            Path::new(path!("/symbols")).join(name),
            file_content::encode_text(text, encoding, bom),
        )
        .await;
    }
    let project = Project::test(fs, [path!("/symbols").as_ref()], cx).await;
    project
        .read_with(cx, |project, _| project.languages().clone())
        .add(cpp_language());
    let buffer = project
        .update(cx, |project, cx| {
            project.open_local_buffer(path!("/symbols/main.cpp"), cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();
    for (receiver, header) in [
        ("one", "crlf.hpp"),
        ("two", "bom.hpp"),
        ("three", "utf16.hpp"),
    ] {
        let offset = text.find(&format!("{receiver}.reset")).unwrap() + receiver.len() + 1;
        let definitions = project
            .update(cx, |project, cx| project.definitions(&buffer, offset, cx))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(definitions.len(), 1);
        definitions[0].target.buffer.read_with(cx, |target, _| {
            assert_eq!(target.file().unwrap().path().as_unix_str(), header);
            let snapshot = target.snapshot();
            let range = definitions[0].target.range.to_offset(&snapshot);
            assert_eq!(snapshot.text().get(range), Some("reset"));
            assert_eq!(snapshot.line_ending(), LineEnding::Windows);
        });
    }
}

#[gpui::test]
async fn syntax_symbols_closed_files_follow_language_settings_and_modelines(
    cx: &mut TestAppContext,
) {
    let (fs, project, buffer) = setup(cx).await;
    cx.update(|cx| {
        settings::SettingsStore::update_global(cx, |store, cx| {
            store
                .set_user_settings(
                    r#"{"file_types":{"C++":["custom-header"]},"modeline_lines":66}"#,
                    cx,
                )
                .unwrap();
        });
    });
    fs.insert_tree(
        path!("/symbols"),
        json!({
            "settings.custom-header": "struct SettingsType {};",
            "modeline.data": "// -*- mode: C++; -*-\nstruct ModelineType {};",
        }),
    )
    .await;
    let trailer = format!(
        "{}struct LateType {{}};\n// vim: set ft=cpp :\n",
        "// filler\n".repeat(1024)
    );
    fs.insert_file(path!("/symbols/trailer.data"), trailer.into_bytes())
        .await;
    let utf16 = "// -*- mode: C++; -*-\nstruct WideType {};"
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect();
    fs.insert_file(path!("/symbols/wide.data"), utf16).await;
    let crlf = format!(
        "//{}\r\n{}// -*- mode: C++; -*-\r\nstruct CrlfType {{}};\r\n{}",
        "x".repeat(916),
        "\r\n".repeat(64),
        "// filler\r\n".repeat(600),
    );
    fs.insert_file(path!("/symbols/crlf.data"), crlf.into_bytes())
        .await;
    cx.run_until_parked();
    let text = concat!(
        "SettingsType settings; ModelineType modeline; LiveWidget widget; ",
        "LateType trailer; WideType wide; CrlfType crlf;",
    );
    buffer.update(cx, |buffer, cx| buffer.set_text(text, cx));

    let header = project
        .update(cx, |project, cx| {
            project.open_local_buffer(path!("/symbols/widget.hpp"), cx)
        })
        .await
        .unwrap();
    header.update(cx, |buffer, cx| {
        buffer.set_language(Some(language::PLAIN_TEXT.clone()), cx);
        buffer.set_text("struct LiveWidget {};", cx);
    });

    for (name, path) in [
        ("SettingsType", "settings.custom-header"),
        ("ModelineType", "modeline.data"),
        ("LiveWidget", "widget.hpp"),
        ("LateType", "trailer.data"),
        ("WideType", "wide.data"),
        ("CrlfType", "crlf.data"),
    ] {
        let offset = text.find(name).unwrap() + 1;
        let locations = project
            .update(cx, |project, cx| project.definitions(&buffer, offset, cx))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(locations.len(), 1);
        locations[0].target.buffer.read_with(cx, |buffer, _| {
            assert_eq!(buffer.file().unwrap().path().as_unix_str(), path);
        });
    }
}
