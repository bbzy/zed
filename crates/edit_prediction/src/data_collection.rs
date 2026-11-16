use crate::{EditPredictionStore, zeta};

use anyhow::Result;
use gpui::{Context, Entity, Task};
use language::{Buffer, ToPoint as _};
use std::{fmt::Write as _, ops::Range};
use text::Point;

pub use zeta_prompt::udiff::CURSOR_POSITION_MARKER;

pub(crate) struct CapturedPredictionContext {
    pub(crate) repository_url: Option<String>,
    pub(crate) revision: Option<String>,
    pub(crate) uncommitted_diff: Option<String>,
    pub(crate) buffer_diagnostics: Vec<zeta_prompt::ActiveBufferDiagnostic>,
    pub(crate) editable_context: Vec<zeta_prompt::RelatedFile>,
}

pub(crate) fn capture_prediction_context(
    buffer: Entity<Buffer>,
    cursor_anchor: language::Anchor,
    editable_context_task: Task<Result<Vec<zeta_prompt::RelatedFile>>>,
    cx: &mut Context<EditPredictionStore>,
) -> Option<Task<Result<CapturedPredictionContext>>> {
    let snapshot = buffer.read(cx).snapshot();
    snapshot.file()?;

    Some(cx.spawn(async move |_this, _cx| {
        let buffer_diagnostics = zeta::active_buffer_diagnostics(
            &snapshot,
            Point::new(0, 0)..snapshot.max_point(),
            cursor_anchor.to_point(&snapshot).row,
            100,
        );
        let editable_context = match editable_context_task.await {
            Ok(editable_context) => editable_context,
            Err(error) => {
                log::debug!("failed to capture editable context: {error:?}");
                Vec::new()
            }
        };

        Ok(CapturedPredictionContext {
            repository_url: None,
            revision: None,
            uncommitted_diff: None,
            buffer_diagnostics,
            editable_context,
        })
    }))
}
pub fn compute_cursor_excerpt(
    snapshot: &language::BufferSnapshot,
    cursor_anchor: language::Anchor,
) -> (String, usize, Range<Point>) {
    use text::ToOffset as _;
    use text::ToPoint as _;

    let cursor_offset = cursor_anchor.to_offset(snapshot);
    let (excerpt_point_range, excerpt_offset_range, cursor_offset_in_excerpt) =
        crate::cursor_excerpt::compute_cursor_excerpt(snapshot, cursor_offset);
    let syntax_ranges = crate::cursor_excerpt::compute_syntax_ranges(
        snapshot,
        cursor_offset,
        &excerpt_offset_range,
    );
    let excerpt_text: String = snapshot.text_for_range(excerpt_point_range).collect();
    let (_, context_range) = zeta_prompt::compute_editable_and_context_ranges(
        &excerpt_text,
        cursor_offset_in_excerpt,
        &syntax_ranges,
        100,
        50,
    );
    let context_text = excerpt_text[context_range.clone()].to_string();
    let cursor_in_context = cursor_offset_in_excerpt.saturating_sub(context_range.start);
    let context_buffer_start =
        (excerpt_offset_range.start + context_range.start).to_point(snapshot);
    let context_buffer_end = (excerpt_offset_range.start + context_range.end).to_point(snapshot);
    (
        context_text,
        cursor_in_context,
        context_buffer_start..context_buffer_end,
    )
}
pub fn format_cursor_excerpt(
    excerpt: &str,
    cursor_offset: usize,
    line_comment_prefix: &str,
) -> String {
    let cursor_line_start = excerpt[..cursor_offset]
        .rfind('\n')
        .map(|pos| pos + 1)
        .unwrap_or(0);
    let cursor_line_end = excerpt[cursor_line_start..]
        .find('\n')
        .map(|pos| cursor_line_start + pos + 1)
        .unwrap_or(excerpt.len());
    let cursor_line = &excerpt[cursor_line_start..cursor_line_end];
    let cursor_line_indent = &cursor_line[..cursor_line.len() - cursor_line.trim_start().len()];
    let cursor_column = cursor_offset - cursor_line_start;

    let mut marker_line = String::new();
    if cursor_column < line_comment_prefix.len() {
        for _ in 0..cursor_column {
            marker_line.push(' ');
        }
        marker_line.push_str(line_comment_prefix);
        write!(marker_line, " <{}", CURSOR_POSITION_MARKER).unwrap();
    } else {
        if cursor_column >= cursor_line_indent.len() + line_comment_prefix.len() {
            marker_line.push_str(cursor_line_indent);
        }
        marker_line.push_str(line_comment_prefix);
        while marker_line.len() < cursor_column {
            marker_line.push(' ');
        }
        write!(marker_line, "^{}", CURSOR_POSITION_MARKER).unwrap();
    }

    let mut result = String::with_capacity(excerpt.len() + marker_line.len() + 2);
    result.push_str(&excerpt[..cursor_line_end]);
    if !result.ends_with('\n') {
        result.push('\n');
    }
    result.push_str(&marker_line);
    if cursor_line_end < excerpt.len() {
        result.push('\n');
        result.push_str(&excerpt[cursor_line_end..]);
    }
    result
}
