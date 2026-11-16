use gpui::App;

pub mod clone;

mod kk_blame;
mod kk_classification;
mod kk_comparison;
mod kk_conflict;
mod kk_diff;
mod kk_diff_controls;
mod kk_file_order;
mod kk_multi_diff;
mod kk_output;
pub mod kk_panel;
mod kk_revision_picker;
mod kk_search;
mod kk_settings;

pub use diff_ui::picker_prompt;
pub(crate) use diff_ui::revision_metadata as commit_metadata;
pub use kk_blame::KkBlameStatus;
pub use kk_diff_controls::{KkDiffActionsToolbar, KkDiffStyleToolbar};

pub fn init(cx: &mut App) {
    kk_blame::init(cx);
    kk_panel::init(cx);
    kk_conflict::init(cx);
    kk_diff::init(cx);
    kk_search::init(cx);
}
