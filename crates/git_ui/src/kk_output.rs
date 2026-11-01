use super::kk_panel::ShowOutput;
use editor::Editor;
use futures::{FutureExt as _, future::Shared};
use gpui::{
    Action, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable, Pixels,
    Task, Window,
};
use kk_workbench::Diagnostics;
use language::{Buffer, Capability};
use multi_buffer::MultiBuffer;
use ui::{IconButton, Tooltip, prelude::*};
use workspace::dock::{DockPosition, Panel, PanelEvent};

pub(super) struct KkOutput {
    diagnostics: Diagnostics,
    categories: Vec<String>,
    buffer: Entity<Buffer>,
    editor: Entity<Editor>,
    focus: FocusHandle,
    position: DockPosition,
    cursor: usize,
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
        Self {
            diagnostics,
            categories: categories.to_vec(),
            buffer,
            editor,
            focus: cx.focus_handle(),
            position: DockPosition::Bottom,
            cursor: 0,
            remote: None,
            configuration: Task::ready(()).shared(),
            remote_updates: Task::ready(()),
            _updates: cx.spawn(async move |this, cx| {
                while updates.recv().await.is_ok() {
                    if this.update(cx, |this, cx| this.synchronize(cx)).is_err() {
                        break;
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
        self.remote_updates = cx.spawn(async move |this, cx| {
            configured.await;
            let mut cursor = None;
            let mut previous_error = None;
            loop {
                let output = remote.diagnostics(cursor, true).await;
                let failed = output.is_err();
                if this
                    .update(cx, |this, cx| match output {
                        Ok(output) => {
                            if cursor.is_some_and(|previous| !output.cursor.same_stream(previous)) {
                                let categories = this.categories.clone();
                                this.configure(&categories, cx);
                            }

                            cursor = Some(output.cursor);
                            previous_error = None;
                            this.diagnostics.append_output(&output.text);
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

    fn synchronize(&mut self, cx: &mut App) {
        let (cursor, text) = self.diagnostics.read_since(self.cursor);
        if !text.is_empty() {
            self.buffer.update(cx, |buffer, cx| {
                buffer.append(text, cx);
            });
        }

        self.cursor = cursor;
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
        v_flex()
            .id("kk-output")
            .key_context("KKWorkbenchOutput")
            .track_focus(&self.focus)
            .size_full()
            .child(
                h_flex()
                    .justify_between()
                    .px_2()
                    .py_1()
                    .child(Label::new("KK Workbench Output"))
                    .child(
                        IconButton::new("close-output", IconName::Close)
                            .tooltip(Tooltip::text("Close Output"))
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(PanelEvent::Close))),
                    ),
            )
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
            assert_eq!(output.cursor, 3);
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
    }
}
