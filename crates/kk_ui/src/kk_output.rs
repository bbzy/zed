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
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use ui::{IconButton, Tooltip, prelude::*};
use workspace::ToolbarItemView as _;
use workspace::dock::{DockPosition, Panel, PanelEvent};

#[derive(Clone, Copy)]
enum OutputCategory {
    Performance,
    Command,
    Error,
    Other,
}

impl OutputCategory {
    const ALL: [Self; 4] = [Self::Performance, Self::Command, Self::Error, Self::Other];
    const ALL_MASK: u8 = 15;

    fn mask(self) -> u8 {
        1 << self as u8
    }

    fn label(self) -> &'static str {
        match self {
            Self::Performance => "PERF",
            Self::Command => "COMMAND",
            Self::Error => "ERROR",
            Self::Other => "Other",
        }
    }
}

struct OutputStream {
    pending_line: String,
    record_categories: u8,
}

impl Default for OutputStream {
    fn default() -> Self {
        Self {
            pending_line: String::new(),
            record_categories: OutputCategory::Other.mask(),
        }
    }
}

impl OutputStream {
    fn filter(&mut self, text: &str, selected: u8) -> String {
        if selected == OutputCategory::ALL_MASK {
            return text.to_owned();
        }

        let mut output = String::new();
        for part in text.split_inclusive('\n') {
            self.pending_line.push_str(part);
            if !part.ends_with('\n') {
                continue;
            }

            let line = std::mem::take(&mut self.pending_line);
            if let Some((_, message)) = line
                .strip_prefix('[')
                .and_then(|line| line.split_once("] "))
                .filter(|(timestamp, _)| OffsetDateTime::parse(timestamp, &Rfc3339).is_ok())
            {
                self.record_categories = if message.starts_with("[PERF] ") {
                    OutputCategory::Performance.mask()
                } else if message.starts_with("[COMMAND] ") || message.starts_with("[COMMAND END] ")
                {
                    OutputCategory::Command.mask()
                } else if let Some(error) = message.strip_prefix("[ERROR] ") {
                    OutputCategory::Error.mask()
                        | if error.starts_with("[COMMAND ") {
                            OutputCategory::Command.mask()
                        } else {
                            0
                        }
                } else {
                    OutputCategory::Other.mask()
                };
            }
            if self.record_categories & selected != 0 {
                output.push_str(&line);
            }
        }
        output
    }
}

pub(super) struct KkOutput {
    diagnostics: Diagnostics,
    categories: Vec<String>,
    buffer: Entity<Buffer>,
    editor: Entity<Editor>,
    search_bar: Entity<BufferSearchBar>,
    focus: FocusHandle,
    position: DockPosition,
    cursor: (usize, usize),
    start_cursor: (usize, usize),
    selected_categories: u8,
    stream: OutputStream,
    replay: Task<()>,
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
        cx.observe(&editor, |_, _, cx| cx.notify()).detach();
        Self {
            diagnostics,
            categories: categories.to_vec(),
            buffer,
            editor,
            search_bar,
            focus: cx.focus_handle(),
            position: DockPosition::Bottom,
            cursor: (0, 0),
            start_cursor: (0, 0),
            selected_categories: OutputCategory::ALL_MASK,
            stream: OutputStream::default(),
            replay: Task::ready(()),
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
        let text = self.stream.filter(&text, self.selected_categories);
        if !text.is_empty() {
            self.buffer.update(cx, |buffer, cx| {
                buffer.append(text, cx);
            });
        }

        self.cursor = cursor;
        more
    }

    fn set_filter(&mut self, selected: u8, cx: &mut Context<Self>) {
        if self.selected_categories == selected {
            return;
        }

        self.selected_categories = selected;
        self.cursor = self.start_cursor;
        self.stream = OutputStream::default();
        self.buffer.update(cx, |buffer, cx| buffer.set_text("", cx));
        self.replay = cx.spawn(async move |this, cx| {
            loop {
                match this.update(cx, |this, cx| this.synchronize(cx)) {
                    Ok(true) => futures_lite::future::yield_now().await,
                    Ok(false) | Err(_) => break,
                }
            }
        });
        cx.notify();
    }

    fn clear(&mut self, cx: &mut Context<Self>) {
        // Advance past buffered batches as well, so pending updates cannot restore old output.
        self.cursor = self.diagnostics.end_cursor();
        self.start_cursor = self.cursor;
        self.stream = OutputStream::default();
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
                                IconButton::new("wrap-output", IconName::TextWrap)
                                    .toggle_state(!matches!(
                                        self.editor.read(cx).soft_wrap_mode(cx),
                                        editor::SoftWrap::None
                                    ))
                                    .tooltip(Tooltip::text("Toggle Soft Wrap"))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.editor.update(cx, |editor, cx| {
                                            editor.toggle_soft_wrap(
                                                &editor::actions::ToggleSoftWrap,
                                                window,
                                                cx,
                                            );
                                        });
                                    })),
                            )
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
            .child(
                h_flex()
                    .px_2()
                    .gap_1()
                    .child(
                        div().debug_selector(|| "output-filter-all".into()).child(
                            ui::Button::new("output-filter-all", "All")
                                .toggle_state(self.selected_categories == OutputCategory::ALL_MASK)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.set_filter(OutputCategory::ALL_MASK, cx);
                                })),
                        ),
                    )
                    .children(OutputCategory::ALL.into_iter().map(|category| {
                        div().debug_selector(move || category.label().into()).child(
                            ui::Button::new(category.label(), category.label())
                                .toggle_state(self.selected_categories & category.mask() != 0)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.set_filter(this.selected_categories ^ category.mask(), cx);
                                })),
                        )
                    })),
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

    #[test]
    fn output_filters_classify_record_headers_and_keep_continuations() {
        assert_eq!(
            OutputStream::default().filter("partial output", OutputCategory::ALL_MASK),
            "partial output"
        );
        let diagnostics = Diagnostics::default();
        diagnostics.log("[PERF] {\"message\":\"[COMMAND] needle 三🙂\"}");
        diagnostics.log("[COMMAND] start");
        diagnostics.log("[COMMAND END] done");
        diagnostics.error("[COMMAND command-id] failed\ncontinuation\n[PERF] error detail");
        diagnostics.error("unrelated error");
        diagnostics.log("other [PERF] message\n[invalid] [COMMAND] still other");
        let text = diagnostics.read_since(0).1;
        for selected in 0..=OutputCategory::ALL_MASK {
            let mut stream = OutputStream::default();
            let mut filtered = String::new();
            for character in text.chars() {
                filtered.push_str(&stream.filter(&character.to_string(), selected));
            }
            assert_eq!(
                filtered.contains("needle"),
                selected & OutputCategory::Performance.mask() != 0
            );
            assert_eq!(
                filtered.contains("start"),
                selected & OutputCategory::Command.mask() != 0
            );
            assert_eq!(
                filtered.contains("done"),
                selected & OutputCategory::Command.mask() != 0
            );
            let command_error =
                selected & (OutputCategory::Command.mask() | OutputCategory::Error.mask()) != 0;
            assert_eq!(
                filtered.matches("failed").count(),
                usize::from(command_error)
            );
            assert_eq!(
                filtered.contains("continuation\n[PERF] error detail"),
                command_error
            );
            assert_eq!(
                filtered.contains("unrelated error"),
                selected & OutputCategory::Error.mask() != 0
            );
            assert_eq!(
                filtered.contains("[invalid] [COMMAND] still other"),
                selected & OutputCategory::Other.mask() != 0
            );
            if selected == OutputCategory::ALL_MASK {
                assert_eq!(filtered, text);
            }
        }
    }

    #[gpui::test(iterations = 3)]
    async fn output_filters_and_wrap_controls_preserve_log_text(cx: &mut TestAppContext) {
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
        let (diagnostics, editor, buffer) = output.read_with(cx, |output, _| {
            (
                output.diagnostics.clone(),
                output.editor.clone(),
                output.buffer.clone(),
            )
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_panel(output.clone(), window, cx);
            workspace.focus_panel::<KkOutput>(window, cx);
        });
        let long_line = "三🙂".repeat(20_000);
        diagnostics.log(format!("[PERF] {long_line}"));
        diagnostics.log("[COMMAND] command");
        diagnostics.error("[COMMAND command-id] failure");
        diagnostics.log("other");
        cx.run_until_parked();
        let original = buffer.read_with(cx, |buffer, _| buffer.text());
        let initially_wrapped = editor.read_with(cx, |editor, cx| {
            !matches!(editor.soft_wrap_mode(cx), editor::SoftWrap::None)
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let wrap = cx.debug_bounds("ICON-TextWrap").expect("wrap button");
        cx.simulate_click(wrap.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        editor.read_with(cx, |editor, cx| {
            assert_ne!(
                !matches!(editor.soft_wrap_mode(cx), editor::SoftWrap::None),
                initially_wrapped
            );
        });
        editor.update_in(cx, |editor, window, cx| {
            editor.toggle_soft_wrap(&editor::actions::ToggleSoftWrap, window, cx);
            editor.select_all(&editor::actions::SelectAll, window, cx);
            editor.focus_handle(cx).focus(window, cx);
        });
        cx.dispatch_action(editor::actions::Copy);
        assert_eq!(
            cx.update(|_, cx| cx.read_from_clipboard().and_then(|item| item.text())),
            Some(original)
        );
        for category in ["COMMAND", "ERROR", "Other"] {
            cx.update(|window, cx| window.draw(cx).clear(cx));
            let button = cx.debug_bounds(category).expect("category button");
            cx.simulate_click(button.center(), gpui::Modifiers::none());
            cx.run_until_parked();
        }
        let performance = buffer.read_with(cx, |buffer, _| buffer.text());
        assert!(performance.contains(&long_line));
        assert!(!performance.contains("command"));
        output.update(cx, |output, cx| {
            output.set_filter(
                OutputCategory::Command.mask() | OutputCategory::Error.mask(),
                cx,
            );
        });
        cx.run_until_parked();
        let commands = buffer.read_with(cx, |buffer, _| buffer.text());
        assert_eq!(commands.matches("failure").count(), 1);
        assert!(!commands.contains(&long_line));
        output.update(cx, |output, cx| output.set_filter(0, cx));
        diagnostics.log("[COMMAND] hidden while disabled");
        cx.run_until_parked();
        assert!(buffer.read_with(cx, |buffer, _| buffer.text()).is_empty());
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let all = cx
            .debug_bounds("output-filter-all")
            .expect("all categories button");
        cx.simulate_click(all.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert!(
            buffer
                .read_with(cx, |buffer, _| buffer.text())
                .contains("hidden while disabled")
        );
        output.update(cx, |output, cx| {
            output.set_filter(OutputCategory::Performance.mask(), cx);
            assert!(output.synchronize(cx));
            output.set_filter(OutputCategory::Command.mask(), cx);
            output.clear(cx);
            output.set_filter(OutputCategory::ALL_MASK, cx);
        });
        diagnostics.append_output("after clear\n");
        cx.run_until_parked();
        assert_eq!(
            buffer.read_with(cx, |buffer, _| buffer.text()),
            "after clear\n"
        );
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.close_panel::<KkOutput>(window, cx);
            workspace.reveal_panel::<KkOutput>(window, cx);
        });
        output.read_with(cx, |output, cx| {
            assert_eq!(output.selected_categories, OutputCategory::ALL_MASK);
            assert_eq!(
                !matches!(
                    output.editor.read(cx).soft_wrap_mode(cx),
                    editor::SoftWrap::None
                ),
                initially_wrapped
            );
            assert!(output.editor.read(cx).read_only(cx));
            assert!(!output.buffer.read(cx).is_dirty());
        });
    }

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
        output.update(cx, |output, cx| output.set_filter(0, cx));
        cx.run_until_parked();
        editor.update(cx, |editor, cx| {
            assert!(editor.search_background_highlights(cx).is_empty())
        });
        output.update(cx, |output, cx| {
            output.set_filter(OutputCategory::ALL_MASK, cx)
        });
        cx.run_until_parked();
        editor.update(cx, |editor, cx| {
            assert_eq!(editor.search_background_highlights(cx).len(), 2)
        });
        central_editor.update(cx, |editor, cx| {
            assert!(editor.search_background_highlights(cx).is_empty())
        });

        // Appending output leaves the caret past the matches; start the round trip on a match.
        cx.dispatch_action(search::SelectNextMatch);
        cx.run_until_parked();
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
