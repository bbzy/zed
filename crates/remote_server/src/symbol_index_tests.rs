use crate::remote_editing_tests::init_test_with_fs;
use fs::FakeFs;
use gpui::{Entity, TestAppContext};
use language::{Buffer, Language, LanguageConfig, LanguageMatcher, OffsetRangeExt};
use project::Project;
use serde_json::json;
use std::sync::Arc;
use util::rel_path::rel_path;

async fn setup(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) -> (Entity<Project>, Entity<Buffer>) {
    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(
        "/remote-symbols",
        json!({
            "include": {"widget.hpp": "struct Widget { void reset(); int field; };"},
            "src": {
                "widget.cpp": "#include \"../include/widget.hpp\"\nvoid Widget::reset() {}",
                "main.cpp": "#include \"../include/widget.hpp\"\nvoid run(Widget widget) { widget.reset(); }"
            },
            ".gitignore": "ignored.hpp\n",
            "ignored.hpp": "struct Ignored {};"
        }),
    )
    .await;
    let (project, headless) = init_test_with_fs(fs, cx, server_cx).await;
    let language = Arc::new(Language::new(
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
    ));
    let server_languages = headless.read_with(server_cx, |project, _| project.languages.clone());
    assert!(
        server_languages
            .language_for_name("C++")
            .await
            .unwrap()
            .grammar()
            .is_some()
    );
    project.read_with(cx, |project, _| project.languages().add(language));
    let worktree = project
        .update(cx, |project, cx| {
            project.find_or_create_worktree("/remote-symbols", true, cx)
        })
        .await
        .unwrap()
        .0;
    let worktree_id = worktree.read_with(cx, |worktree, _| worktree.id());
    let buffer = project
        .update(cx, |project, cx| {
            project.open_buffer((worktree_id, rel_path("src/main.cpp")), cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();
    (project, buffer)
}

#[gpui::test(iterations = 10)]
async fn syntax_symbols_remote_navigation_references_and_completions(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let (project, buffer) = setup(cx, server_cx).await;
    let offset = buffer.read_with(cx, |buffer, _| buffer.text().rfind("reset").unwrap());
    let definitions = project
        .update(cx, |project, cx| project.definitions(&buffer, offset, cx))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(definitions.len(), 1);
    definitions[0].target.buffer.read_with(cx, |buffer, _| {
        assert!(buffer.text().contains("Widget::reset"));
        assert_eq!(
            &buffer.text()[definitions[0].target.range.to_offset(&buffer.snapshot())],
            "reset"
        );
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
            project.completions(
                &buffer,
                offset + 2,
                lsp::CompletionContext {
                    trigger_kind: lsp::CompletionTriggerKind::INVOKED,
                    trigger_character: None,
                },
                cx,
            )
        })
        .await
        .unwrap();
    let completion = completions
        .iter()
        .flat_map(|response| &response.completions)
        .find(|completion| completion.new_text == "reset")
        .unwrap();
    assert_eq!(completion.kind(), Some(lsp::CompletionItemKind::METHOD));
    assert!(completion.source.server_id().is_none());
    assert!(completion.label.text.contains("reset()"));
    let snapshot = buffer.read_with(cx, |buffer, _| buffer.snapshot());
    assert_eq!(
        completion.replace_range.to_offset(&snapshot),
        offset..offset + 5
    );
}

#[gpui::test(iterations = 10)]
async fn syntax_symbols_remote_unsaved_overlays_and_missing_symbols(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let (project, buffer) = setup(cx, server_cx).await;
    let header = project
        .update(cx, |project, cx| {
            project.open_local_buffer("/remote-symbols/include/widget.hpp", cx)
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
    for text in [
        "void run() { Ignored value; }",
        "void run() { unknown.reset(); }",
        "// reset",
    ] {
        buffer.update(cx, |buffer, cx| buffer.set_text(text, cx));
        let offset = text.find("Ignored").or_else(|| text.find("reset")).unwrap();
        assert!(
            project
                .update(cx, |project, cx| project.definitions(&buffer, offset, cx))
                .await
                .unwrap()
                .is_none()
        );
    }
}
