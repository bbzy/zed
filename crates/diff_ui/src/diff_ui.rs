use gpui::{App, TaskExt as _};
use workspace::Workspace;

pub mod file_diff_view;
pub mod multi_diff_view;
pub mod picker_prompt;
pub mod revision_metadata;
pub mod text_diff_view;

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(
            |workspace, action: &editor::actions::DiffClipboardWithSelectionData, window, cx| {
                if let Some(task) =
                    text_diff_view::TextDiffView::open(action, workspace, window, cx)
                {
                    task.detach_and_log_err(cx);
                }
            },
        );
    })
    .detach();
}
