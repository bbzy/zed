use super::kk_panel::ShowOutput;
use editor::Editor;
use futures::{FutureExt as _, future::Shared};
use gpui::{
    Action, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    KeyContext, Pixels, Task, Window,
};
use kk_workbench::Diagnostics;
use language::{Buffer, Capability};
use multi_buffer::MultiBuffer;
use search::{BufferSearchBar, buffer_search::DivRegistrar};
use ui::{IconButton, Tooltip, prelude::*};
use workspace::ToolbarItemView as _;
use workspace::dock::{DockPosition, Panel, PanelEvent};

pub(super) struct KkOutput {
    diagnostics: Diagnostics,
    categories: Vec<String>,
    buffer: Entity<Buffer>,
    editor: Entity<Editor>,
    search_bar: Entity<BufferSearchBar>,
    focus: FocusHandle,
    position: DockPosition,
    cursor: (usize, usize),
    remote: Option<kk_workbench::remote::Client>,
    configuration: Shared<Task<()>>,
    remote_updates: Task<()>,
    _updates: Task<()>,
}

impl KkOutput {
    pub fn new(categories: &[String], window: &mut Window, cx: &mut Context<Self>) -> Self {
        let diagnostics = Diagnostics::default();
        diagnostics.configure(categories);
        let updates = diagnostics.subscribe();
        let buffer = cx.new(|cx| {
            let mut buffer = Buffer::local("", cx);
            buffer.set_capability(Capability::ReadOnly, cx);
            buffer
        });
        let multibuffer = cx.new(|cx| MultiBuffer::singleton(buffer.clone(), cx));
        let editor = cx.new(|cx| {
            let mut editor = Editor::for_multibuffer(multibuffer, None, window, cx);
            editor.set_read_only(true);
            editor.set_should_serialize(false, cx);
            editor
        });
        let search_bar = cx.new(|cx| {
            let mut search_bar = BufferSearchBar::new(None, window, cx);
            search_bar.set_active_pane_item(Some(&editor), window, cx);
            search_bar
        });
        cx.observe(&search_bar, |_, _, cx| cx.notify()).detach();
        Self {
            diagnostics,
            categories: categories.to_vec(),
            buffer,
            editor,
            search_bar,
            focus: cx.focus_handle(),
            position: DockPosition::Bottom,
            cursor: (0, 0),
            remote: None,
            configuration: Task::ready(()).shared(),
            remote_updates: Task::ready(()),
            _updates: cx.spawn(async move |this, cx| {
                while updates.recv().await.is_ok() {
                    loop {
                        match this.update(cx, |this, cx| this.synchronize(cx)) {
                            Ok(true) => futures_lite::future::yield_now().await,
                            Ok(false) => break,
                            Err(_) => return,
                        }
                    }
                }
            }),
        }
    }

    pub fn diagnostics(&self) -> &Diagnostics {
        &self.diagnostics
    }

    pub fn connect(
        &mut self,
        remote: kk_workbench::remote::Client,
        categories: &[String],
        cx: &mut Context<Self>,
    ) {
        self.remote = Some(remote.clone());
        self.configure(categories, cx);
        let configured = self.configuration.clone();
        let diagnostics = self.diagnostics.clone();
        self.remote_updates = cx.spawn(async move |this, cx| {
            configured.await;
            let mut cursor = None;
            let mut previous_error = None;
            loop {
                let output = cx
                    .background_spawn({
                        let remote = remote.clone();
                        let diagnostics = diagnostics.clone();
                        async move {
                            let output = remote.diagnostics(cursor, true).await?;
                            diagnostics.append_output(&output.text);
                            anyhow::Ok(output.cursor)
                        }
                    })
                    .await;
                let failed = output.is_err();
                if this
                    .update(cx, |this, cx| match output {
                        Ok(next_cursor) => {
                            if cursor.is_some_and(|previous| !next_cursor.same_stream(previous)) {
                                let categories = this.categories.clone();
                                this.configure(&categories, cx);
                            }

                            cursor = Some(next_cursor);
                            previous_error = None;
                        }
                        Err(error) => {
                            let error = format!("Could not read remote KK output: {error:#}");
                            if previous_error.as_ref() != Some(&error) {
                                this.diagnostics.error(&error);
                                previous_error = Some(error);
                            }
                        }
                    })
                    .is_err()
                {
                    break;
                }

                if failed {
                    cx.background_executor()
                        .timer(std::time::Duration::from_secs(1))
                        .await;
                }
            }
        });
    }

    pub fn configure(&mut self, categories: &[String], cx: &mut Context<Self>) {
        self.diagnostics.configure(categories);
        self.categories = categories.to_vec();
        let Some(remote) = self.remote.clone() else {
            return;
        };
        let categories = categories.to_vec();
        let previous = self.configuration.clone();
        let diagnostics = self.diagnostics.clone();
        self.configuration = cx
            .spawn(async move |_, _| {
                previous.await;
                if let Err(error) = remote.configure_diagnostics(categories).await {
                    diagnostics.error(format!("Could not configure remote KK output: {error:#}"));
                }
            })
            .shared();
    }

    fn synchronize(&mut self, cx: &mut App) -> bool {
        let (cursor, text, more) = self.diagnostics.read_batch(self.cursor, 64 * 1024);
        if !text.is_empty() {
            self.buffer.update(cx, |buffer, cx| {
                buffer.append(text, cx);
            });
        }

        self.cursor = cursor;
        more
    }

    fn clear(&mut self, cx: &mut Context<Self>) {
        // Advance past buffered batches as well, so pending updates cannot restore old output.
        self.cursor = self.diagnostics.end_cursor();
        self.buffer.update(cx, |buffer, cx| {
            buffer.set_text("", cx);
        });
    }
}

impl Focusable for KkOutput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl EventEmitter<PanelEvent> for KkOutput {}

impl Panel for KkOutput {
    fn persistent_name() -> &'static str {
        "KK Workbench Output"
    }
    fn panel_key() -> &'static str {
        "KKWorkbenchOutput"
    }
    fn activation_focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
    fn position(&self, _: &Window, _: &App) -> DockPosition {
        self.position
    }
    fn position_is_valid(&self, _: DockPosition) -> bool {
        true
    }
    fn set_position(&mut self, position: DockPosition, _: &mut Window, cx: &mut Context<Self>) {
        self.position = position;
        cx.notify();
    }
    fn default_size(&self, _: &Window, _: &App) -> Pixels {
        px(240.)
    }
    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        None
    }
    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("KK Workbench Output")
    }
    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ShowOutput)
    }
    fn activation_priority(&self) -> u32 {
        0
    }
}

impl Render for KkOutput {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut key_context = KeyContext::new_with_defaults(cx.platform_info().os);
        key_context.add("KKWorkbenchOutput");
        self.search_bar
            .read(cx)
            .contribute_context(&mut key_context, cx);
        let mut registrar =
            DivRegistrar::new(|this: &Self, _, _| Some(this.search_bar.clone()), cx);
        BufferSearchBar::register(&mut registrar);

        registrar
            .into_div()
            .flex()
            .flex_col()
            .id("kk-output")
            .key_context(key_context)
            .track_focus(&self.focus)
            .size_full()
            .child(
                h_flex()
                    .justify_between()
                    .px_2()
                    .py_1()
                    .child(Label::new("KK Workbench Output"))
                    .child(
                        h_flex()
                            .child(
                                IconButton::new("search-output", IconName::MagnifyingGlass)
                                    .tooltip(Tooltip::text("Search Output"))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.search_bar.update(cx, |search_bar, cx| {
                                            search_bar.deploy(
                                                &search::buffer_search::Deploy::find(),
                                                None,
                                                window,
                                                cx,
                                            );
                                        });
                                    })),
                            )
                            .child(
                                IconButton::new("clear-output", IconName::Trash)
                                    .tooltip(Tooltip::text("Clear Output"))
                                    .on_click(cx.listener(|this, _, _, cx| this.clear(cx))),
                            )
                            .child(
                                IconButton::new("close-output", IconName::Close)
                                    .tooltip(Tooltip::text("Close Output"))
                                    .on_click(
                                        cx.listener(|_, _, _, cx| cx.emit(PanelEvent::Close)),
                                    ),
                            ),
                    ),
            )
            .when(!self.search_bar.read(cx).is_dismissed(), |this| {
                this.child(self.search_bar.clone())
            })
            .child(div().flex_1().min_h_0().child(self.editor.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use project::{FakeFs, Project};
    use settings::SettingsStore;
    use workspace::MultiWorkspace;

    #[gpui::test(iterations = 3)]
    async fn output_stream_preserves_focus_readonly_state_and_history_when_reopened(
        cx: &mut TestAppContext,
    ) {
        cx.update(|cx| {
            let settings = SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/project", serde_json::json!({})).await;
        let project = Project::test(fs, ["/project".as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = multi_workspace.read_with(cx, |view, _| view.workspace().clone());
        let output = cx.update(|window, cx| cx.new(|cx| KkOutput::new(&[], window, cx)));
        let diagnostics = output.read_with(cx, |output, _| output.diagnostics.clone());
        diagnostics.log("before opening");
        cx.run_until_parked();
        let active_editor = cx.update(|window, cx| {
            let buffer = cx.new(|cx| Buffer::local("current draft", cx));
            let editor = cx.new(|cx| Editor::for_buffer(buffer, None, window, cx));
            workspace.update(cx, |workspace, cx| {
                workspace.add_item_to_active_pane(Box::new(editor.clone()), None, true, window, cx);
                workspace.add_panel(output.clone(), window, cx);
            });
            editor
        });
        cx.run_until_parked();
        let focus = active_editor.read_with(cx, |editor, cx| editor.focus_handle(cx));
        cx.update(|window, cx| {
            focus.focus(window, cx);
            workspace.update(cx, |workspace, cx| {
                workspace.reveal_panel::<KkOutput>(window, cx)
            });
            assert!(focus.is_focused(window));
            workspace.update(cx, |workspace, cx| {
                workspace.reveal_panel::<KkOutput>(window, cx)
            });
        });
        cx.run_until_parked();
        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
        let editor = output.read_with(cx, |output, _| output.editor.clone());
        let buffer = output.read_with(cx, |output, _| output.buffer.clone());
        pane.read_with(cx, |pane, _| assert_eq!(pane.items_len(), 1));
        cx.update(|window, cx| {
            assert!(editor.read(cx).read_only(cx));
            assert!(!buffer.read(cx).is_dirty());
            assert!(buffer.read(cx).text().contains("before opening"));
            assert!(focus.is_focused(window));
        });
        diagnostics.log("while open");
        cx.run_until_parked();
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.close_panel::<KkOutput>(window, cx)
            });
        });
        cx.run_until_parked();
        diagnostics.log("while closed");
        cx.run_until_parked();
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.reveal_panel::<KkOutput>(window, cx)
            });
            let output = output.read(cx);
            let text = buffer.read(cx).text();
            assert_eq!(text.matches("before opening").count(), 1);
            assert_eq!(text.matches("while open").count(), 1);
            assert_eq!(text.matches("while closed").count(), 1);
            assert_eq!(output.cursor, (3, 0));
            assert!(!buffer.read(cx).is_dirty());
            assert_eq!(pane.read(cx).items_len(), 1);
            assert_eq!(
                workspace
                    .read(cx)
                    .active_item(cx)
                    .and_then(|item| item.downcast::<Editor>()),
                Some(active_editor.clone())
            );
        });
        let large_output = "三🙂\n".repeat(40_000);
        diagnostics.append_output(&large_output);
        cx.run_until_parked();
        cx.update(|window, cx| {
            assert!(buffer.read(cx).text().ends_with(&large_output));
            assert_eq!(output.read(cx).cursor, (4, 0));
            assert!(!buffer.read(cx).is_dirty());
            assert!(editor.read(cx).read_only(cx));
            assert!(focus.is_focused(window));
        });

        diagnostics.append_output(&large_output);
        output.update(cx, |output, cx| {
            assert!(output.synchronize(cx));
            assert_ne!(output.cursor.1, 0);
            output.clear(cx);
            assert_eq!(buffer.read(cx).text(), "");
        });
        diagnostics.append_output("after clearing\n");
        cx.run_until_parked();
        cx.update(|window, cx| {
            assert_eq!(buffer.read(cx).text(), "after clearing\n");
            assert!(editor.read(cx).read_only(cx));
            assert!(!buffer.read(cx).is_dirty());
            assert!(focus.is_focused(window));
            assert!(diagnostics.read_since(0).1.contains("before opening"));
            workspace.update(cx, |workspace, cx| {
                workspace.close_panel::<KkOutput>(window, cx);
                workspace.reveal_panel::<KkOutput>(window, cx);
            });
        });
        cx.run_until_parked();
        assert_eq!(
            buffer.read_with(cx, |buffer, _| buffer.text()),
            "after clearing\n"
        );
    }

    #[gpui::test(iterations = 3)]
    async fn output_search_stays_in_panel_and_updates_after_append_and_clear(
        cx: &mut TestAppContext,
    ) {
        cx.update(|cx| {
            let settings = SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            search::init(cx);
            cx.bind_keys([
                gpui::KeyBinding::new(
                    "ctrl-f",
                    search::buffer_search::Deploy::find(),
                    Some("Editor"),
                ),
                gpui::KeyBinding::new("escape", editor::actions::Cancel, Some("Editor")),
            ]);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/project", serde_json::json!({})).await;
        let project = Project::test(fs, ["/project".as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = multi_workspace.read_with(cx, |view, _| view.workspace().clone());
        let output = cx.update(|window, cx| cx.new(|cx| KkOutput::new(&[], window, cx)));
        let (diagnostics, editor, search_bar) = output.read_with(cx, |output, _| {
            (
                output.diagnostics.clone(),
                output.editor.clone(),
                output.search_bar.clone(),
            )
        });
        let central_editor = cx.update(|window, cx| {
            let buffer = cx.new(|cx| Buffer::local("central needle", cx));
            let central_editor = cx.new(|cx| Editor::for_buffer(buffer, None, window, cx));
            workspace.update(cx, |workspace, cx| {
                workspace.add_item_to_active_pane(
                    Box::new(central_editor.clone()),
                    None,
                    true,
                    window,
                    cx,
                );
                workspace.add_panel(output.clone(), window, cx);
                workspace.focus_panel::<KkOutput>(window, cx);
            });
            central_editor
        });
        diagnostics.append_output("needle first\nneedle second\nNEEDLE third\nneedles\n");
        cx.run_until_parked();
        cx.simulate_keystrokes("ctrl-f");
        cx.run_until_parked();
        assert!(!search_bar.read_with(cx, |search_bar, _| search_bar.is_dismissed()));
        search_bar
            .update_in(cx, |bar, window, cx| {
                bar.search(
                    "needle",
                    Some(search::SearchOptions::CASE_SENSITIVE | search::SearchOptions::WHOLE_WORD),
                    false,
                    window,
                    cx,
                )
            })
            .await
            .expect("search output");
        cx.run_until_parked();
        editor.update(cx, |editor, cx| {
            assert_eq!(editor.search_background_highlights(cx).len(), 2)
        });
        central_editor.update(cx, |editor, cx| {
            assert!(editor.search_background_highlights(cx).is_empty())
        });

        let selection_before = editor.update(cx, |editor, cx| {
            editor
                .selections
                .newest::<language::Point>(&editor.display_snapshot(cx))
        });
        cx.dispatch_action(search::SelectNextMatch);
        cx.run_until_parked();
        let selection_after = editor.update(cx, |editor, cx| {
            editor
                .selections
                .newest::<language::Point>(&editor.display_snapshot(cx))
        });
        assert_ne!(selection_before.range(), selection_after.range());
        cx.dispatch_action(search::SelectPreviousMatch);
        cx.run_until_parked();
        assert_eq!(
            selection_before.range(),
            editor.update(cx, |editor, cx| editor
                .selections
                .newest::<language::Point>(&editor.display_snapshot(cx))
                .range())
        );

        diagnostics.append_output("needle new\n");
        cx.run_until_parked();
        editor.update(cx, |editor, cx| {
            assert_eq!(editor.search_background_highlights(cx).len(), 3)
        });
        output.update(cx, |output, cx| output.clear(cx));
        cx.run_until_parked();
        editor.update(cx, |editor, cx| {
            assert!(editor.search_background_highlights(cx).is_empty())
        });
        diagnostics.append_output("needle after clear\n");
        cx.run_until_parked();
        editor.update(cx, |editor, cx| {
            assert_eq!(editor.search_background_highlights(cx).len(), 1)
        });

        search_bar
            .update_in(cx, |bar, window, cx| {
                bar.search(
                    "n[a-z]+",
                    Some(search::SearchOptions::REGEX | search::SearchOptions::CASE_SENSITIVE),
                    false,
                    window,
                    cx,
                )
            })
            .await
            .expect("regex search output");
        editor.update(cx, |editor, cx| {
            assert_eq!(editor.search_background_highlights(cx).len(), 1)
        });
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        cx.update(|window, cx| {
            assert!(search_bar.read(cx).is_dismissed());
            assert!(editor.focus_handle(cx).is_focused(window));
            assert!(editor.read(cx).read_only(cx));
        });
    }
}
