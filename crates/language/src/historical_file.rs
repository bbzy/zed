use crate::{
    Buffer, Capability, DiskState, File, LanguageRegistry, LineEnding, LocalFile, ReplicaId, Rope,
    TextBuffer,
};
use anyhow::Result;
use gpui::{App, AppContext as _, AsyncWindowContext, Entity};
use settings::WorktreeId;
use std::{path::PathBuf, sync::Arc};
use util::{ResultExt as _, paths::PathStyle, rel_path::RelPath};

pub struct HistoricalFile {
    pub path: Arc<RelPath>,
    pub worktree_id: WorktreeId,
    pub is_deleted: bool,
    pub is_binary: bool,
    pub display_name: String,
}

impl File for HistoricalFile {
    fn as_local(&self) -> Option<&dyn LocalFile> {
        None
    }

    fn disk_state(&self) -> DiskState {
        DiskState::Historic {
            was_deleted: self.is_deleted,
        }
    }

    fn path_style(&self, _: &App) -> PathStyle {
        PathStyle::local()
    }

    fn path(&self) -> &Arc<RelPath> {
        &self.path
    }

    fn full_path(&self, _: &App) -> PathBuf {
        if self.path.is_empty() {
            PathBuf::from(".")
        } else {
            self.path.as_std_path().to_path_buf()
        }
    }

    fn file_name<'a>(&'a self, _: &'a App) -> &'a str {
        self.display_name.as_ref()
    }

    fn worktree_id(&self, _: &App) -> WorktreeId {
        self.worktree_id
    }

    fn to_proto(&self, _cx: &App) -> crate::proto::File {
        unimplemented!()
    }

    fn is_private(&self) -> bool {
        false
    }

    fn can_open(&self) -> bool {
        !self.is_binary
    }
}

pub async fn build_buffer(
    mut text: String,
    blob: Arc<dyn File>,
    language_registry: &Arc<LanguageRegistry>,
    cx: &mut AsyncWindowContext,
) -> Result<Entity<Buffer>> {
    let line_ending = LineEnding::detect(&text);
    LineEnding::normalize(&mut text);
    let text = Rope::from(text);
    let language =
        cx.update(|_, cx| language_registry.language_for_file(&blob, Some(&text), cx))?;
    let language = if let Some(language_id) = language {
        language_registry
            .load_language(language_id)
            .await
            .ok()
            .and_then(|e| e.log_err())
    } else {
        None
    };
    let buffer = cx.new(|cx| {
        let buffer = TextBuffer::new_normalized(
            ReplicaId::LOCAL,
            cx.entity_id().as_non_zero_u64().into(),
            line_ending,
            text,
        );
        let mut buffer = Buffer::build(buffer, Some(blob), Capability::ReadWrite, cx);
        buffer.set_language_registry(language_registry.clone());
        buffer.set_language_async(language, cx);
        buffer
    });
    Ok(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{File, Language, LanguageConfig, LanguageMatcher, markdown_lang};
    use gpui::{EmptyView, TestAppContext};
    use indoc::indoc;
    use settings::SettingsStore;

    #[gpui::test]
    async fn test_build_buffer_resolves_injected_languages(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
        });

        let language_registry = Arc::new(LanguageRegistry::test(cx.executor()));
        language_registry.add(markdown_lang());
        language_registry.add(Arc::new(markdown_inline_lang()));

        let window = cx.add_window(|_, _| EmptyView);
        let mut async_cx = window
            .update(cx, |_, window, cx| window.to_async(cx))
            .expect("window should be open");

        let text = indoc! {"
            # Title

            Some *emphasized* text.
        "}
        .to_string();
        let blob = Arc::new(HistoricalFile {
            path: util::rel_path::RelPath::from_unix_str("notes.md")
                .unwrap()
                .into(),
            worktree_id: WorktreeId::from_usize(0),
            is_deleted: false,
            is_binary: false,
            display_name: "abc1234 - notes.md".into(),
        }) as Arc<dyn File>;

        let buffer = build_buffer(text, blob, &language_registry, &mut async_cx)
            .await
            .expect("buffer should build");

        cx.run_until_parked();

        buffer.read_with(cx, |buffer, _| {
            let language = buffer.language().expect("buffer should have a language");
            assert_eq!(language.name().as_ref(), "Markdown");

            let layers = buffer
                .snapshot()
                .syntax_layers()
                .map(|layer| layer.language.name().to_string())
                .collect::<Vec<_>>();
            assert!(
                layers.iter().any(|name| name == "Markdown-Inline"),
                "emphasis, links, and fenced code blocks are highlighted by the injected \
                 Markdown-Inline grammar, but the buffer parsed with layers {layers:?}",
            );
        });
    }

    #[gpui::test]
    fn historical_file_language_detection_ignores_display_name(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let mut store = SettingsStore::test(cx);
            store
                .set_user_settings(r#"{"file_types":{"JSONC":["tsconfig*.json"]}}"#, cx)
                .expect("valid file type settings");
            cx.set_global(store);
        });

        let language_registry = Arc::new(LanguageRegistry::test(cx.executor()));
        language_registry.register_test_language(LanguageConfig {
            name: "JSON".into(),
            matcher: Arc::new(LanguageMatcher {
                path_suffixes: vec!["json".into(), "flake.lock".into()],
                ..LanguageMatcher::default()
            }),
            ..LanguageConfig::default()
        });
        language_registry.register_test_language(LanguageConfig {
            name: "JSONC".into(),
            ..LanguageConfig::default()
        });

        let languages = cx.update(|cx| {
            [
                ("flake.lock", "abc1234 - flake.lock"),
                ("flake.lock", "flake.lock @ abc1234"),
                ("nix/flake.lock", "abc1234 - flake.lock"),
                ("nix/flake.lock", "flake.lock @ abc1234"),
                ("web/tsconfig.json", "abc1234 - tsconfig.json"),
                ("web/tsconfig.json", "tsconfig.json @ abc1234"),
            ]
            .map(|(path, display_name)| {
                let blob = Arc::new(HistoricalFile {
                    path: util::rel_path::RelPath::from_unix_str(path)
                        .expect("valid repository path")
                        .into(),
                    worktree_id: WorktreeId::from_usize(0),
                    is_deleted: false,
                    is_binary: false,
                    display_name: display_name.to_string(),
                }) as Arc<dyn File>;
                assert_eq!(blob.file_system_abs_path(cx), None);
                language_registry
                    .language_for_file(&blob, None, cx)
                    .and_then(|id| language_registry.language_name_for_id(id))
                    .map(|name| name.to_string())
            })
        });

        assert_eq!(
            languages.each_ref().map(|name| name.as_deref()),
            [
                Some("JSON"),
                Some("JSON"),
                Some("JSON"),
                Some("JSON"),
                Some("JSONC"),
                Some("JSONC"),
            ]
        );
    }

    fn markdown_inline_lang() -> Language {
        Language::new(
            LanguageConfig {
                name: "Markdown-Inline".into(),
                hidden: true,
                ..LanguageConfig::default()
            },
            Some(tree_sitter_md::INLINE_LANGUAGE.into()),
        )
        .with_highlights_query("(emphasis) @emphasis")
        .unwrap()
    }
}
