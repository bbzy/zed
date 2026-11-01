use super::kk_diff::DiffTarget;
use db::kvp::KeyValueStore;
use editor::{Editor, EditorEvent};
use futures::{FutureExt as _, future::Shared};
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, KeyBinding, ScrollHandle,
    Subscription, Task, WeakEntity, Window, actions,
};
use kk_workbench::{
    ChangedFile, ChangedLine, LineKind, SearchOptions, SearchResults, Snapshot, search_diff,
};
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use ui::{Button, ListItem, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{Item, Workspace, item::ItemEvent};

actions!(
    kk_workbench,
    [
        SearchInChanges,
        SearchInRevision,
        SearchSelectionInChanges,
        PreviousSearchResult,
        NextSearchResult,
        OpenSearchResult,
        FocusSearchQuery,
        FocusSearchResults
    ]
);

const HISTORY_KEY: &str = "KKWorkbenchSearchHistory";

pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("up", PreviousSearchResult, Some("KKDiffSearch")),
        KeyBinding::new("down", NextSearchResult, Some("KKDiffSearch")),
        KeyBinding::new("enter", OpenSearchResult, Some("KKDiffSearch")),
        KeyBinding::new("escape", FocusSearchQuery, Some("KKDiffSearch")),
        KeyBinding::new("tab", FocusSearchResults, Some("KKDiffSearch")),
    ]);
}

pub(super) struct DiffSearch {
    snapshot: Snapshot,
    target: DiffTarget,
    workspace: WeakEntity<Workspace>,
    query: Entity<Editor>,
    results_focus: FocusHandle,
    options: SearchOptions,
    lines: Arc<Vec<ChangedLine>>,
    files: HashMap<String, ChangedFile>,
    results: SearchResults,
    selected: usize,
    history: Vec<String>,
    history_index: Option<usize>,
    history_draft: String,
    completed_query: String,
    generation: u64,
    loading: bool,
    data_valid: bool,
    searching: bool,
    error: Option<String>,
    cancelled: Arc<AtomicBool>,
    load_task: Task<()>,
    search_task: Task<()>,
    query_performance: Option<kk_workbench::PerformanceSpan>,
    presentation_context: Option<kk_workbench::PerformanceContext>,
    history_write: Shared<Task<()>>,
    scroll: ScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl DiffSearch {
    pub fn open(
        snapshot: Snapshot,
        target: DiffTarget,
        initial_query: Option<String>,
        workspace: Entity<Workspace>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let view = cx.new(|cx| {
            let mut view = Self::new(
                snapshot,
                target,
                initial_query,
                workspace.downgrade(),
                window,
                cx,
            );
            view.load(cx);
            view
        });
        workspace.update(cx, |workspace, cx| {
            if !workspace.activate_item(&view, true, true, window, cx) {
                workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
            }
        });
    }

    fn new(
        snapshot: Snapshot,
        target: DiffTarget,
        initial_query: Option<String>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let query = cx.new(|cx| {
            let mut editor = Editor::auto_height(1, 6, window, cx);
            editor.set_placeholder_text("Search changed lines (↑/↓ for history)", window, cx);
            if let Some(query) = initial_query {
                editor.set_text(query, window, cx);
            }

            editor
        });
        let subscription =
            cx.subscribe_in(
                &query,
                window,
                |view: &mut Self, _, event, _, cx| match event {
                    EditorEvent::BufferEdited => {
                        if view.history_index.and_then(|index| view.history.get(index))
                            != Some(&view.query.read(cx).text(cx))
                        {
                            view.history_index = None;
                        }

                        view.search(cx);
                    }
                    EditorEvent::Blurred => view.remember(cx),
                    _ => {}
                },
            );
        Self {
            snapshot,
            target,
            workspace,
            query,
            results_focus: cx.focus_handle(),
            options: SearchOptions::default(),
            lines: Arc::new(Vec::new()),
            files: HashMap::new(),
            results: SearchResults::default(),
            selected: 0,
            history: Vec::new(),
            history_index: None,
            history_draft: String::new(),
            completed_query: String::new(),
            generation: 0,
            loading: true,
            data_valid: false,
            searching: false,
            error: None,
            cancelled: Arc::new(AtomicBool::new(false)),
            load_task: Task::ready(()),
            search_task: Task::ready(()),
            query_performance: None,
            presentation_context: None,
            history_write: Task::ready(()).shared(),
            scroll: ScrollHandle::new(),
            _subscriptions: vec![subscription],
        }
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        self.cancelled.store(true, Ordering::Relaxed);
        self.loading = true;
        self.data_valid = false;
        self.error = None;
        let snapshot = self.snapshot.clone();
        let performance = snapshot.repository().diagnostics().start("background.searchLoad", None, serde_json::json!({"root": snapshot.repository().root(), "panel": cx.entity_id().as_u64()}));
        let snapshot = snapshot.with_performance_context(performance.context().cloned());
        let target = self.target.clone();
        let store = KeyValueStore::global(cx);
        let history_write = self.history_write.clone();
        self.load_task = cx.spawn(async move |this, cx| {
            history_write.await;
            let loaded = cx
                .background_spawn(async move {
                    let (target, _) = target.resolve(&snapshot).await?;
                    let scope = &target.scope;
                    let (lines, files) = snapshot.search_data(scope).await?;
                    let history = store
                        .read_kvp(HISTORY_KEY)?
                        .and_then(|text| serde_json::from_str::<Vec<String>>(&text).ok())
                        .unwrap_or_default();
                    anyhow::Ok((lines, files, history, target))
                })
                .await;
            performance.finish(
                if loaded.is_ok() { "completed" } else { "error" },
                serde_json::json!({"boundary": "search data loaded"}),
            );
            this.update(cx, |this, cx| {
                this.loading = false;
                match loaded {
                    Ok((lines, files, history, target)) => {
                        this.data_valid = true;
                        this.target = target;
                        this.lines = Arc::new(lines);
                        this.files = files
                            .into_iter()
                            .map(|file| (file.path.clone(), file))
                            .collect();
                        this.history = history;
                        this.search(cx);
                    }
                    Err(error) => this.error = Some(format!("{error:#}")),
                }

                cx.notify();
            })
            .log_err();
        });
        cx.notify();
    }

    fn search(&mut self, cx: &mut Context<Self>) {
        if let Some(performance) = self.query_performance.take() {
            performance.finish("superseded", serde_json::Value::Null);
        }
        self.presentation_context = None;
        self.generation += 1;
        self.cancelled.store(true, Ordering::Relaxed);
        self.cancelled = Arc::new(AtomicBool::new(false));
        self.completed_query.clear();
        if self.loading || !self.data_valid {
            return;
        }

        self.error = None;
        let query = self.query.read(cx).text(cx);
        if query.is_empty() {
            self.results = SearchResults::default();
            self.searching = false;
            cx.notify();
            return;
        }

        let generation = self.generation;
        let lines = self.lines.clone();
        let cancelled = self.cancelled.clone();
        let options = self.options;
        let diagnostics = self.snapshot.repository().diagnostics().clone();
        let performance = diagnostics.start(
            "search.query",
            None,
            serde_json::json!({"panel": cx.entity_id().as_u64(), "generation": generation}),
        );
        let context = performance.context().cloned();
        self.query_performance = Some(performance);
        let selected = self
            .results
            .indices
            .get(self.selected)
            .and_then(|index| lines.get(*index))
            .cloned();
        self.searching = true;
        self.search_task = cx.spawn(async move |this, cx| {
            let (query, result) = cx
                .background_spawn(async move {
                    let worker = diagnostics.start("search.worker", context.as_ref(), serde_json::json!({"lines": lines.len()}));
                    let result = search_diff(&lines, &query, options, &cancelled);
                    worker.finish(if result.is_ok() { "completed" } else if cancelled.load(Ordering::Relaxed) { "cancelled" } else { "error" }, serde_json::Value::Null);
                    (query, result)
                })
                .await;
            this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }

                this.searching = false;
                let outcome = if result.is_ok() { "completed" } else { "error" };
                match result {
                    Ok(results) => {
                        this.selected = selected
                            .and_then(|selected| {
                                results.indices.iter().position(|index| {
                                    this.lines.get(*index).is_some_and(|line| {
                                        line.path == selected.path
                                            && line.kind == selected.kind
                                            && line.line_number == selected.line_number
                                            && line.content == selected.content
                                    })
                                })
                            })
                            .unwrap_or(this.selected)
                            .min(results.indices.len().saturating_sub(1));
                        this.results = results;
                        this.completed_query = query;
                    }
                    Err(error) => this.error = Some(format!("{error:#}")),
                }

                if let Some(performance) = this.query_performance.take() {
                    this.presentation_context = performance.context().cloned();
                    performance.finish(outcome, serde_json::json!({"resultCount": this.results.count, "boundary": "result model updated"}));
                }

                cx.notify();
            })
            .log_err();
        });
        cx.notify();
    }

    fn remember(&mut self, cx: &mut Context<Self>) {
        if self.completed_query.is_empty() {
            return;
        }

        let query = std::mem::take(&mut self.completed_query);
        self.history.retain(|entry| *entry != query);
        self.history.insert(0, query);
        self.history.truncate(20);
        let history = self.history.clone();
        let store = KeyValueStore::global(cx);
        let previous = self.history_write.clone();
        self.history_write = cx
            .spawn(async move |this, cx| {
                previous.await;
                let result = cx
                    .background_spawn(async move {
                        store
                            .write_kvp(HISTORY_KEY.into(), serde_json::to_string(&history)?)
                            .await
                    })
                    .await;
                if let Err(error) = result {
                    this.update(cx, |this, cx| {
                        this.error = Some(format!("Could not save search history: {error:#}"));
                        cx.notify();
                    })
                    .log_err();
                }
            })
            .shared();
        let write = self.history_write.clone();
        cx.spawn(async move |_, _| write.await).detach();
    }

    fn navigate(&mut self, previous: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.query.focus_handle(cx).contains_focused(window, cx) {
            if self.history.is_empty() {
                return;
            }

            if self.history_index.is_none() {
                self.history_draft = self.query.read(cx).text(cx);
            }

            self.history_index = if previous {
                Some(
                    self.history_index
                        .map_or(0, |index| index + 1)
                        .min(self.history.len() - 1),
                )
            } else {
                self.history_index.and_then(|index| index.checked_sub(1))
            };
            let query = self
                .history_index
                .and_then(|index| self.history.get(index))
                .unwrap_or(&self.history_draft)
                .clone();
            self.query
                .update(cx, |editor, cx| editor.set_text(query, window, cx));
        } else {
            self.selected = if previous {
                self.selected.saturating_sub(1)
            } else {
                (self.selected + 1).min(self.results.indices.len().saturating_sub(1))
            };
            self.scroll.scroll_to_item(self.selected);
            cx.notify();
        }
    }

    fn open_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.loading || !self.data_valid || self.searching || self.error.is_some() {
            return;
        }

        let Some(line) = self
            .results
            .indices
            .get(self.selected)
            .and_then(|index| self.lines.get(*index))
            .cloned()
        else {
            return;
        };
        if !self.files.contains_key(&line.path) {
            return;
        }
        self.remember(cx);
        let panel = self
            .workspace
            .upgrade()
            .and_then(|workspace| workspace.read(cx).panel::<super::kk_panel::KkPanel>(cx));
        let Some(panel) = panel else {
            self.error = Some("KK Workbench was closed".into());
            cx.notify();
            return;
        };
        let open = panel.update(cx, |panel, cx| {
            panel.open_search_diff_task(
                self.snapshot.clone(),
                self.target.clone(),
                line.path.clone(),
                window,
                cx,
            )
        });
        cx.spawn_in(window, async move |this, cx| match open.await {
            Ok(Some(view)) => {
                view.update_in(cx, |view, window, cx| view.reveal_line(&line, window, cx))
                    .log_err();
            }
            Ok(None) => {}
            Err(error) => {
                this.update(cx, |this, cx| {
                    this.error = Some(format!("{error:#}"));
                    cx.notify();
                })
                .log_err();
            }
        })
        .detach();
    }
}

impl Drop for DiffSearch {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}
impl EventEmitter<()> for DiffSearch {}
impl Focusable for DiffSearch {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.query.focus_handle(cx)
    }
}
impl Item for DiffSearch {
    type Event = ();
    fn tab_content_text(&self, _: usize, _: &App) -> SharedString {
        format!("Search Diff: {}", self.target.label).into()
    }
    fn to_item_events(_: &(), _: &mut dyn FnMut(ItemEvent)) {}
}

impl Render for DiffSearch {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let presentation = self.presentation_context.take().map(|context| {
            self.snapshot.repository().diagnostics().start(
                "search.presentation",
                Some(&context),
                serde_json::Value::Null,
            )
        });
        let status = if let Some(error) = &self.error {
            error.clone()
        } else if self.loading {
            "Loading diff…".into()
        } else if self.searching {
            "Searching…".into()
        } else if self.query.read(cx).text(cx).is_empty() {
            "Type to search · ↑/↓ for history".into()
        } else {
            format!(
                "{} results{} · Tab to results, Enter to open",
                self.results.count,
                if self.results.count > 500 {
                    " · Showing first 500; refine your search"
                } else {
                    ""
                }
            )
        };
        let mut previous_block = None;
        let rows = self
            .results
            .indices
            .iter()
            .enumerate()
            .filter_map(|(index, line_index)| {
                let line = self.lines.get(*line_index)?;
                let group_start = previous_block != Some(line.edit_block);
                previous_block = Some(line.edit_block);
                let context = line.context.clone();
                Some(
                    div().when(group_start, |view| view.mt_3()).child(
                        ListItem::new(index)
                            .toggle_state(index == self.selected)
                            .child(
                                v_flex().child(Label::new(line.path.clone())).child(
                                    Label::new(format!(
                                        "{}: {} {}",
                                        line.line_number,
                                        if line.kind == LineKind::Added {
                                            "+"
                                        } else {
                                            "−"
                                        },
                                        line.content
                                    ))
                                    .color(if line.kind == LineKind::Added {
                                        Color::Success
                                    } else {
                                        Color::Error
                                    })
                                    .truncate(),
                                ),
                            )
                            .tooltip(Tooltip::text(context))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.selected = index;
                                this.open_selected(window, cx);
                            })),
                    ),
                )
            })
            .collect::<Vec<_>>();
        let element =
            v_flex()
                .size_full()
                .key_context("KKDiffSearch")
                .track_focus(&self.results_focus)
                .on_action(cx.listener(|this, _: &PreviousSearchResult, window, cx| {
                    this.navigate(true, window, cx)
                }))
                .on_action(cx.listener(|this, _: &NextSearchResult, window, cx| {
                    this.navigate(false, window, cx)
                }))
                .on_action(cx.listener(|this, _: &OpenSearchResult, window, cx| {
                    this.open_selected(window, cx)
                }))
                .on_action(cx.listener(|this, _: &FocusSearchQuery, window, cx| {
                    window.focus(&this.query.focus_handle(cx), cx)
                }))
                .on_action(cx.listener(|this, _: &FocusSearchResults, window, cx| {
                    window.focus(&this.results_focus, cx)
                }))
                .child(
                    h_flex()
                        .p_2()
                        .gap_1()
                        .child(div().flex_1().child(self.query.clone()))
                        .child(
                            Button::new("case", "Aa")
                                .toggle_state(self.options.case_sensitive)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.options.case_sensitive = !this.options.case_sensitive;
                                    this.search(cx);
                                    window.focus(&this.query.focus_handle(cx), cx);
                                })),
                        )
                        .child(
                            Button::new("word", "ab")
                                .toggle_state(self.options.whole_word)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.options.whole_word = !this.options.whole_word;
                                    this.search(cx);
                                    window.focus(&this.query.focus_handle(cx), cx);
                                })),
                        )
                        .child(
                            Button::new("regex", ".*")
                                .toggle_state(self.options.regex)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.options.regex = !this.options.regex;
                                    this.search(cx);
                                    window.focus(&this.query.focus_handle(cx), cx);
                                })),
                        ),
                )
                .child(
                    div()
                        .px_2()
                        .child(Label::new(status).color(if self.error.is_some() {
                            Color::Error
                        } else {
                            Color::Muted
                        })),
                )
                .child(
                    v_flex()
                        .id("diff-search-results")
                        .flex_1()
                        .overflow_y_scroll()
                        .track_scroll(&self.scroll)
                        .children(rows),
                );
        if let Some(presentation) = presentation {
            presentation.finish(
                "completed",
                serde_json::json!({"boundary": "GPUI element tree built"}),
            );
        }
        element
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kk_panel::remote_tests::run_kk;
    use gpui::{TestAppContext, VisualContext};
    use kk_workbench::Repository;
    use project::{FakeFs, Project};
    use settings::SettingsStore;
    use std::path::Path;
    use workspace::MultiWorkspace;

    #[gpui::test(iterations = 3)]
    async fn kk_remote_search_navigation_resolves_rewritten_history_and_missing_results(
        cx: &mut TestAppContext,
        server_cx: &mut TestAppContext,
    ) {
        use crate::kk_panel::{KkPanel, remote_tests::RemoteFixture};
        let fixture = RemoteFixture::create(cx, server_cx)
            .await
            .expect("remote fixture");
        run_kk(&fixture.root, &["new", "-m", "work"])
            .await
            .expect("historical source");
        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            MultiWorkspace::test_new(fixture.project.clone(), window, cx)
        });
        let workspace = multi_workspace.read_with(cx, |view, _| view.workspace().clone());
        let panel = cx
            .update(|window, cx| KkPanel::load(workspace.downgrade(), window.to_async(cx)))
            .await
            .expect("panel");
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_panel(panel.clone(), window, cx)
        });
        let remote = fixture
            .project
            .read_with(cx, |project, cx| project.kk_workbench_client(cx))
            .expect("transport");
        let repository = remote
            .open(&fixture.root, Path::new("kk"), Default::default())
            .await
            .expect("repository");
        let snapshot = repository.snapshot(false).await.expect("initial snapshot");
        let revision = snapshot.exact_revision("@-").await.expect("source");
        let search = cx.new_window_entity(|window, cx| {
            let mut search = DiffSearch::new(
                snapshot.clone(),
                DiffTarget::change(&revision),
                Some("captured".into()),
                workspace.downgrade(),
                window,
                cx,
            );
            search.load(cx);
            search
        });
        cx.condition(&search, |search, _| !search.loading && !search.searching)
            .await;
        search.read_with(cx, |search, _| {
            assert!(search.error.is_none(), "{:?}", search.error);
            assert_eq!(search.results.count, 1);
        });
        run_kk(&fixture.root, &["edit", &revision.revset()])
            .await
            .expect("edit source");
        std::fs::write(fixture.root.join("file.txt"), "rewritten history\n")
            .expect("rewrite contents");
        run_kk(&fixture.root, &["new", "-m", "later work"])
            .await
            .expect("record rewrite");
        let opened = panel
            .update_in(cx, |panel, window, cx| {
                panel.open_search_diff_task(
                    snapshot.clone(),
                    DiffTarget::change(&revision),
                    "file.txt".into(),
                    window,
                    cx,
                )
            })
            .await
            .expect("open latest")
            .expect("diff");
        opened.read_with(cx, |view, cx| {
            assert_eq!(view.buffer.read(cx).text(), "rewritten history\n")
        });
        search.read_with(cx, |search, _| {
            assert_eq!(search.snapshot.operation(), snapshot.operation());
            assert_eq!(search.results.count, 1);
            assert!(search.lines.iter().any(|line| line.content == "captured"));
        });
        run_kk(&fixture.root, &["edit", &revision.revset()])
            .await
            .expect("edit source again");
        run_kk(&fixture.root, &["restore", "--from", "@-", "file.txt"])
            .await
            .expect("remove historical difference");
        run_kk(&fixture.root, &["new"])
            .await
            .expect("record removal");
        let missing = panel
            .update_in(cx, |panel, window, cx| {
                panel.open_search_diff_task(
                    snapshot,
                    DiffTarget::change(&revision),
                    "file.txt".into(),
                    window,
                    cx,
                )
            })
            .await;
        assert!(
            missing
                .err()
                .expect("missing result must not reopen stale contents")
                .to_string()
                .contains("no longer contains")
        );
    }

    #[gpui::test(iterations = 5)]
    async fn newer_search_and_failed_refresh_preserve_query_and_result_validity(
        cx: &mut TestAppContext,
    ) {
        cx.background_executor.allow_parking();
        let (directory, snapshot, revision) = cx
            .background_spawn(async move {
                futures_lite::future::block_on(async {
                    let directory = tempfile::tempdir()?;
                    run_kk(directory.path(), &["init", "."]).await?;
                    let snapshot = Repository::open(directory.path(), Path::new("kk"))
                        .await?
                        .snapshot(false)
                        .await?;
                    let revision = snapshot
                        .revisions(Some("@"), 1)
                        .await?
                        .pop()
                        .ok_or_else(|| anyhow::anyhow!("Missing working copy"))?;
                    anyhow::Ok((directory, snapshot, revision))
                })
            })
            .await
            .expect("prepare repository snapshot");
        cx.update(|cx| {
            let settings = SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/project", serde_json::json!({})).await;
        let project = Project::test(fs, [Path::new("/project")], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = multi_workspace.read_with(cx, |workspace, _| workspace.workspace().clone());
        let original_snapshot = snapshot.clone();
        let diagnostics = original_snapshot.repository().diagnostics().clone();
        diagnostics.configure(&["perf".into()]);
        let view = cx.new_window_entity(|window, cx| {
            let mut view = DiffSearch::new(snapshot, DiffTarget::change(&revision), Some("needle".into()), workspace.downgrade(), window, cx);
            view.lines = Arc::new(kk_workbench::parse_git_diff("diff --git a/f b/f\n--- /dev/null\n+++ b/f\n@@ -0,0 +1,2 @@\n+needle\n+newer\n").expect("parse test diff"));
            view.loading = false;
            view.data_valid = true;
            view
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(Box::new(view.clone()), None, true, window, cx)
        });
        view.update(cx, |view, cx| view.search(cx));
        let query = view.read_with(cx, |view, _| view.query.clone());
        query.update_in(cx, |query, window, cx| query.set_text("newer", window, cx));
        assert_eq!(query.read_with(cx, |query, cx| query.text(cx)), "newer");
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(!view.searching);
            assert_eq!(view.results.indices, vec![1]);
            assert_eq!(view.completed_query, "newer");
        });
        view.update(cx, |view, _| view.options.regex = true);
        query.update_in(cx, |query, window, cx| query.set_text("[", window, cx));
        cx.run_until_parked();
        assert!(view.read_with(cx, |view, _| view.error.is_some()));
        assert_eq!(query.read_with(cx, |query, cx| query.text(cx)), "[");
        view.update(cx, |view, _| {
            view.data_valid = false;
            view.error = Some("Refresh failed".into());
        });
        query.update_in(cx, |query, window, cx| {
            query.set_text("retained draft", window, cx)
        });
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |view, _| view.error.clone()),
            Some("Refresh failed".into())
        );
        assert_eq!(
            query.read_with(cx, |query, cx| query.text(cx)),
            "retained draft"
        );
        for text in ["first independent query", "second independent query"] {
            cx.update(|window, cx| {
                DiffSearch::open(
                    original_snapshot.clone(),
                    DiffTarget::change(&revision),
                    Some(text.into()),
                    workspace.clone(),
                    window,
                    cx,
                )
            });
        }
        assert_eq!(
            workspace.read_with(cx, |workspace, cx| workspace
                .items_of_type::<DiffSearch>(cx)
                .count()),
            3
        );
        let searches = workspace.read_with(cx, |workspace, cx| {
            workspace
                .items_of_type::<DiffSearch>(cx)
                .collect::<Vec<_>>()
        });
        for search in &searches {
            cx.condition(search, |search, _| !search.loading).await;
        }
        cx.run_until_parked();
        assert_eq!(
            query.read_with(cx, |query, cx| query.text(cx)),
            "retained draft"
        );
        view.read_with(cx, |view, _| {
            assert_eq!(view.lines.len(), 2);
            assert!(view.options.regex);
            assert_eq!(view.snapshot.operation(), original_snapshot.operation());
        });
        let log = diagnostics.read_since(0).1;
        for stage in [
            "search.query",
            "search.worker",
            "background.searchLoad",
            "parse.searchDiff",
        ] {
            assert!(log.contains(stage), "Missing {stage}: {log}");
        }
        assert!(!log.contains("first independent query"));
        assert!(!log.contains("retained draft"));
        drop(directory);
    }
}
