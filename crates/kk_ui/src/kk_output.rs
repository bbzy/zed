use super::kk_panel::ShowOutput;
use editor::Editor;
use futures::{FutureExt as _, future::Shared};
use gpui::{
    Action, Anchor, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    KeyContext, Pixels, Task, Window,
};
use kk_workbench::Diagnostics;
use language::{Buffer, Capability};
use multi_buffer::MultiBuffer;
use search::{BufferSearchBar, buffer_search::DivRegistrar};
use serde_json::Value;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use ui::{ContextMenu, IconButton, IconPosition, Indicator, PopoverMenu, Tooltip, prelude::*};
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
            Self::Performance => "Performance",
            Self::Command => "Commands",
            Self::Error => "Errors",
            Self::Other => "Other",
        }
    }
}

struct OutputStream {
    pending_line: String,
    streaming_plain_line: bool,
    record_categories: u8,
}

impl Default for OutputStream {
    fn default() -> Self {
        Self {
            pending_line: String::new(),
            streaming_plain_line: false,
            record_categories: OutputCategory::Other.mask(),
        }
    }
}

impl OutputStream {
    fn render(&mut self, text: &str, selected: u8) -> String {
        let mut output = String::new();
        for part in text.split_inclusive('\n') {
            let complete = part.ends_with('\n');
            if selected == OutputCategory::ALL_MASK
                && self.pending_line.is_empty()
                && (self.streaming_plain_line || !part.starts_with('['))
            {
                output.push_str(part);
                self.streaming_plain_line = !complete;
                continue;
            }

            self.pending_line.push_str(part);
            if !complete {
                continue;
            }

            let line = std::mem::take(&mut self.pending_line);
            let mut structured = None;
            if let Some((timestamp, message)) = line
                .strip_prefix('[')
                .and_then(|line| line.split_once("] "))
                .filter(|(timestamp, _)| OffsetDateTime::parse(timestamp, &Rfc3339).is_ok())
            {
                let (categories, record) = classify_output(message);
                self.record_categories = categories;
                structured = record.map(|(category, payload)| (timestamp, category, payload));
            }

            if self.record_categories & selected != 0 {
                if let Some(rendered) = structured.and_then(|(timestamp, category, payload)| {
                    render_logfmt(timestamp, category, payload)
                }) {
                    output.push_str(&rendered);
                    output.push('\n');
                } else {
                    output.push_str(&line);
                }
            }
        }
        output
    }
}

fn classify_output(message: &str) -> (u8, Option<(&str, &str)>) {
    if let Some(payload) = message.strip_prefix("[PERF] ") {
        (OutputCategory::Performance.mask(), Some(("perf", payload)))
    } else if let Some(payload) = message.strip_prefix("[COMMAND] ") {
        (OutputCategory::Command.mask(), Some(("command", payload)))
    } else if let Some(payload) = message.strip_prefix("[COMMAND END] ") {
        (
            OutputCategory::Command.mask(),
            Some(("command_end", payload)),
        )
    } else if let Some(error) = message.strip_prefix("[ERROR] ") {
        let mut categories = OutputCategory::Error.mask();
        if error.starts_with("[COMMAND ") {
            categories |= OutputCategory::Command.mask();
        }
        (categories, None)
    } else {
        (OutputCategory::Other.mask(), None)
    }
}

fn render_logfmt(timestamp: &str, category: &str, payload: &str) -> Option<String> {
    let fields = serde_json::from_str::<serde_json::Map<String, Value>>(payload).ok()?;
    let first_fields: &[&str] = match category {
        "command" => &["executable", "commandId", "cwd", "argv"],
        "command_end" => &["commandId", "exitCode", "elapsedMs"],
        _ => &["name", "event", "outcome", "elapsedMs"],
    };

    let mut output = format!("ts={timestamp} category={category}");
    for key in first_fields {
        if let Some(value) = fields.get(*key) {
            append_logfmt_field(&mut output, key, value)?;
        }
    }
    for (key, value) in &fields {
        if !first_fields.contains(&key.as_str()) {
            append_logfmt_field(&mut output, key, value)?;
        }
    }
    Some(output)
}

fn append_logfmt_field(output: &mut String, key: &str, value: &Value) -> Option<()> {
    if key.is_empty()
        || key.chars().any(|character| {
            character.is_whitespace() || character.is_control() || matches!(character, '=' | '"')
        })
    {
        return None;
    }

    match value {
        Value::Object(fields) if !fields.is_empty() => {
            for (name, value) in fields {
                append_logfmt_field(output, &format!("{key}.{name}"), value)?;
            }
        }
        Value::Array(values) if !values.is_empty() => {
            for (index, value) in values.iter().enumerate() {
                append_logfmt_field(output, &format!("{key}.{index}"), value)?;
            }
        }
        value => {
            output.push(' ');
            output.push_str(key);
            output.push('=');
            if let Value::String(value) = value
                && !value.is_empty()
                && value != "null"
                && !value.chars().any(|character| {
                    character.is_whitespace()
                        || character.is_control()
                        || matches!(character, '=' | '"' | '\\')
                })
            {
                output.push_str(value);
            } else {
                output.push_str(&value.to_string());
            }
        }
    }
    Some(())
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
        let text = self.stream.render(&text, self.selected_categories);
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

    fn render_filter_menu(&self, cx: &Context<Self>) -> PopoverMenu<ContextMenu> {
        let output = cx.entity().downgrade();
        PopoverMenu::new("output-filter")
            .anchor(Anchor::TopRight)
            .trigger_with_tooltip(
                IconButton::new("output-filter-trigger", IconName::Filter).when(
                    self.selected_categories != OutputCategory::ALL_MASK,
                    |button| button.indicator(Indicator::dot()),
                ),
                Tooltip::text("Filter Output"),
            )
            .menu(move |window, cx| {
                let output = output.upgrade()?;
                Some(ContextMenu::build_persistent(
                    window,
                    cx,
                    move |menu, _, cx| {
                        let selected = output.read(cx).selected_categories;
                        let mut menu = menu;
                        for category in OutputCategory::ALL {
                            let output = output.clone();
                            menu = menu.toggleable_entry(
                                category.label(),
                                selected & category.mask() != 0,
                                IconPosition::Start,
                                None,
                                move |_, cx| {
                                    output.update(cx, |output, cx| {
                                        output.set_filter(
                                            output.selected_categories ^ category.mask(),
                                            cx,
                                        );
                                    });
                                },
                            );
                        }

                        let output = output.clone();
                        menu.separator().entry("Show All", None, move |_, cx| {
                            output.update(cx, |output, cx| {
                                output.set_filter(OutputCategory::ALL_MASK, cx);
                            });
                        })
                    },
                ))
            })
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
                            .child(self.render_filter_menu(cx))
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

    #[test]
    fn output_formats_command_and_performance_metadata_as_logfmt() {
        let mut stream = OutputStream::default();
        let timestamp = "2026-11-18T14:32:01+08:00";
        let records = format!(
            "[{timestamp}] [COMMAND] {}\n[{timestamp}] [COMMAND END] {}\n[{timestamp}] [PERF] {}\n",
            serde_json::json!({
                "executable": "/bin/kk",
                "commandId": "8a91",
                "cwd": "/work/三 🙂",
                "argv": ["log", "a=b", "", "\"quoted\"\\path\nnext"],
                "environment": {"COLUMNS": "80"},
                "environmentOverrides": {"COLUMNS": null},
            }),
            serde_json::json!({"commandId": "8a91", "exitCode": 0, "elapsedMs": 12.4, "stdoutBytes": 480}),
            serde_json::json!({"name": "history.refresh", "event": "end", "outcome": "completed", "elapsedMs": 13.8}),
        );
        assert_eq!(
            stream.render(&records, OutputCategory::ALL_MASK),
            format!(
                concat!(
                    "ts={0} category=command executable=/bin/kk commandId=8a91 cwd=\"/work/三 🙂\" ",
                    "argv.0=log argv.1=\"a=b\" argv.2=\"\" argv.3=\"\\\"quoted\\\"\\\\path\\nnext\" ",
                    "environment.COLUMNS=80 environmentOverrides.COLUMNS=null\n",
                    "ts={0} category=command_end commandId=8a91 exitCode=0 elapsedMs=12.4 stdoutBytes=480\n",
                    "ts={0} category=perf name=history.refresh event=end outcome=completed elapsedMs=13.8\n",
                ),
                timestamp,
            )
        );
    }

    #[test]
    fn output_filters_classify_record_headers_and_keep_continuations() {
        assert_eq!(
            OutputStream::default().render("partial output", OutputCategory::ALL_MASK),
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
                filtered.push_str(&stream.render(&character.to_string(), selected));
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
                assert_eq!(filtered, OutputStream::default().render(&text, selected));
                assert!(filtered.contains(r#"category=perf message="[COMMAND] needle 三🙂""#));
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
            cx.bind_keys([gpui::KeyBinding::new("escape", menu::Cancel, Some("menu"))]);
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
        diagnostics.log(format!("[PERF] {}", serde_json::json!({"name": long_line})));
        diagnostics.log("[COMMAND] command");
        diagnostics.error("[COMMAND command-id] failure");
        diagnostics.log("other");
        cx.run_until_parked();
        let original = buffer.read_with(cx, |buffer, _| buffer.text());
        assert!(original.contains(&format!("category=perf name={long_line}")));
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
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("MENU_ITEM-Performance").is_none());
        let filter = cx.debug_bounds("ICON-Filter").expect("filter button");
        cx.simulate_click(filter.center(), gpui::Modifiers::none());
        for category in ["MENU_ITEM-Commands", "MENU_ITEM-Errors", "MENU_ITEM-Other"] {
            cx.update(|window, cx| window.draw(cx).clear(cx));
            let entry = cx.debug_bounds(category).expect("category menu entry");
            cx.simulate_click(entry.center(), gpui::Modifiers::none());
            cx.run_until_parked();
        }

        cx.simulate_keystrokes("escape");
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("MENU_ITEM-Performance").is_none());
        let performance = buffer.read_with(cx, |buffer, _| buffer.text());
        assert!(performance.contains(&long_line));
        assert!(!performance.contains("command"));

        cx.simulate_click(filter.center(), gpui::Modifiers::none());
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let commands = cx
            .debug_bounds("MENU_ITEM-Commands")
            .expect("commands entry");
        cx.simulate_click(commands.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert_eq!(
            output.read_with(cx, |output, _| output.selected_categories),
            OutputCategory::Performance.mask() | OutputCategory::Command.mask(),
        );
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.simulate_click(filter.center(), gpui::Modifiers::none());
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("MENU_ITEM-Commands").is_none());

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
        cx.simulate_click(filter.center(), gpui::Modifiers::none());
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let all = cx
            .debug_bounds("MENU_ITEM-Show All")
            .expect("show all entry");
        cx.simulate_click(all.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        cx.simulate_keystrokes("escape");
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
