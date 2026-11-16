use editor::{SplittableEditor, hover_markdown_style};
use gpui::{AnyElement, AnyView, App, ClipboardItem, Entity, ScrollHandle, Window};
use markdown::{Markdown, MarkdownElement, MarkdownStyle};
use ui::{Disclosure, HighlightedLabel, Tooltip, WithScrollbar, prelude::*};

pub(crate) fn identifier(value: impl Into<gpui::SharedString>) -> Label {
    Label::new(value).color(Color::Accent)
}

pub(crate) fn change_identifier(
    value: impl Into<gpui::SharedString>,
    prefix: &str,
) -> HighlightedLabel {
    let value = value.into();
    let length = if !prefix.is_empty() && value.starts_with(prefix) {
        prefix.len()
    } else {
        0
    };
    HighlightedLabel::new(value, (0..length).collect()).color(Color::Muted)
}

pub(crate) fn person(value: impl Into<gpui::SharedString>) -> Label {
    Label::new(value).color(Color::Info)
}

pub(crate) fn description(value: impl Into<gpui::SharedString>) -> Label {
    Label::new(value).color(Color::Default)
}

pub(crate) fn tooltip(
    fields: Vec<(String, String, Color)>,
) -> impl Fn(&mut Window, &mut App) -> AnyView {
    tooltip_with_prefix(fields, None)
}

pub(crate) fn tooltip_with_prefix(
    fields: Vec<(String, String, Color)>,
    change_id_prefix: Option<String>,
) -> impl Fn(&mut Window, &mut App) -> AnyView {
    Tooltip::element(move |_, _| {
        v_flex()
            .gap_1()
            .max_w(rems(40.))
            .children(fields.iter().map(|(title, value, color)| {
                v_flex()
                    .when(!title.is_empty(), |view| {
                        view.child(
                            Label::new(title.clone())
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                    })
                    .child(if title.ends_with("Change ID") {
                        change_identifier(
                            value.clone(),
                            change_id_prefix.as_deref().unwrap_or_default(),
                        )
                        .into_any_element()
                    } else {
                        Label::new(value.clone()).color(*color).into_any_element()
                    })
            }))
            .into_any_element()
    })
}

pub(crate) const AVATAR_BORDER_WIDTH: Pixels = px(1.);

pub(crate) fn avatar_placeholder(
    size: Option<gpui::AbsoluteLength>,
    window: &Window,
    cx: &App,
) -> AnyElement {
    let container_size =
        size.map(|size| size.to_pixels(window.rem_size()) + AVATAR_BORDER_WIDTH * 2.);
    h_flex()
        .when_some(container_size, |this, size| this.size(size))
        .justify_center()
        .rounded_full()
        .border(AVATAR_BORDER_WIDTH)
        .border_color(cx.theme().colors().border_variant)
        .bg(cx.theme().colors().element_disabled)
        .child(
            Icon::new(IconName::Person)
                .color(Color::Muted)
                .size(IconSize::XSmall),
        )
        .into_any_element()
}

pub(super) struct CommitHeader {
    pub author_name: gpui::SharedString,
    pub author_email: gpui::SharedString,
    pub timestamp: Option<i64>,
    pub description: String,
    pub change_id_prefix: Option<String>,
    pub identifiers: Vec<(&'static str, gpui::SharedString)>,
    pub avatar: AnyElement,
    pub editor: Entity<SplittableEditor>,
    pub message: Entity<Markdown>,
    pub expanded: bool,
    pub message_scroll_handle: ScrollHandle,
    pub on_toggle: std::rc::Rc<dyn Fn(&gpui::ClickEvent, &mut Window, &mut App)>,
}

impl CommitHeader {
    pub fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let date_string = self
            .timestamp
            .and_then(|timestamp| time::OffsetDateTime::from_unix_timestamp(timestamp).ok())
            .map(|date| {
                time_format::format_localized_timestamp(
                    date,
                    time::OffsetDateTime::now_utc(),
                    time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC),
                    time_format::TimestampFormat::MediumAbsolute,
                )
            });
        let avatar_size = rems_from_px(40_f32);
        let avatar_size_px = avatar_size.to_pixels(window.rem_size());
        let gutter_width = self.editor.update(cx, |editor, cx| {
            let editor = editor.rhs_editor().clone();
            editor.update(cx, |editor, cx| {
                let snapshot = editor.snapshot(window, cx);
                let style = editor.style(cx);
                let font_id = window.text_system().resolve_font(&style.text.font());
                let font_size = style.text.font_size.to_pixels(window.rem_size());
                snapshot
                    .gutter_dimensions(font_id, font_size, style, window, cx)
                    .full_width()
            })
        });
        let avatar_min_side_padding = rems_from_px(6_f32).to_pixels(window.rem_size());
        let avatar_container_min = avatar_size_px + avatar_min_side_padding;
        let avatar_container_width = gutter_width.max(avatar_container_min);

        let has_more = self.description.trim().contains('\n');
        let is_expanded = self.expanded;
        let expand_tooltip = if is_expanded {
            "Fold Commit Description"
        } else {
            "Expand Commit Description"
        };

        let message = self.render_commit_message(avatar_container_width, window, cx);
        let on_toggle = self.on_toggle.clone();
        v_flex()
            .debug_selector(|| "commit-details".into())
            .w_full()
            .py_2p5()
            .gap_2()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                h_flex()
                    .pr_2p5()
                    .w_full()
                    .flex_wrap()
                    .justify_between()
                    .child(
                        h_flex()
                            .child(
                                h_flex()
                                    .flex_none()
                                    .w(avatar_container_width)
                                    .justify_center()
                                    .child(self.avatar),
                            )
                            .child(
                                v_flex()
                                    .child(h_flex().gap_1().child(person(self.author_name)).when(
                                        has_more,
                                        |this| {
                                            this.child(
                                                Disclosure::new(
                                                    "commit-message-disclosure",
                                                    is_expanded,
                                                )
                                                .closed_icon(IconName::ExpandVertical)
                                                .opened_icon(IconName::FoldVertical)
                                                .tooltip(Tooltip::text(expand_tooltip))
                                                .on_click(move |event, window, cx| {
                                                    on_toggle(event, window, cx)
                                                }),
                                            )
                                        },
                                    ))
                                    .child(
                                        h_flex()
                                            .gap_1p5()
                                            .when_some(date_string, |this, date| {
                                                this.child(
                                                    Label::new(date)
                                                        .color(Color::Muted)
                                                        .size(LabelSize::Small),
                                                )
                                                .child(
                                                    Label::new("•")
                                                        .size(LabelSize::Small)
                                                        .color(Color::Muted)
                                                        .alpha(0.5),
                                                )
                                            })
                                            .child(
                                                person(self.author_email).size(LabelSize::Small),
                                            ),
                                    ),
                            ),
                    )
                    .child(h_flex().gap_1().children(self.identifiers.into_iter().map(
                        |(label, identifier)| {
                            copy_commit_identifier(
                                label,
                                identifier,
                                self.change_id_prefix.clone(),
                                cx,
                            )
                        },
                    ))),
            )
            .children(message)
    }

    fn render_commit_message(
        &self,
        avatar_spacer: Pixels,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<impl IntoElement + use<>> {
        let message = self.description.trim();
        if message.is_empty() {
            return None;
        }

        let markdown_style = MarkdownStyle {
            soft_break_as_hard_break: true,
            ..hover_markdown_style(window, cx)
        };

        let is_expanded = self.expanded;

        let has_more = message.contains('\n');
        let collapsed = has_more && !is_expanded;
        let collapsed_height = window.line_height();
        let max_expanded_height = window.line_height() * 12.;

        Some(
            h_flex()
                .w_full()
                .pr_2p5()
                .child(h_flex().flex_none().w(avatar_spacer))
                .child(
                    div()
                        .relative()
                        .flex_1()
                        .min_w_0()
                        .child(
                            div()
                                .id("commit-message")
                                .size_full()
                                .text_sm()
                                .when(collapsed, |this| this.h(collapsed_height).overflow_hidden())
                                .when(!collapsed, |this| {
                                    this.max_h(max_expanded_height)
                                        .overflow_y_scroll()
                                        .track_scroll(&self.message_scroll_handle)
                                })
                                .child(MarkdownElement::new(self.message.clone(), markdown_style)),
                        )
                        .vertical_scrollbar_for(&self.message_scroll_handle, window, cx),
                ),
        )
    }
}

fn copy_commit_identifier(
    label: &'static str,
    identifier: gpui::SharedString,
    change_id_prefix: Option<String>,
    cx: &App,
) -> Button {
    let copied = cx
        .read_from_clipboard()
        .and_then(|item| item.text())
        .is_some_and(|text| text.trim() == identifier.as_ref());
    Button::new(label, label)
        .color(Color::Accent)
        .start_icon(
            Icon::new(if copied {
                IconName::Check
            } else {
                IconName::Copy
            })
            .size(IconSize::Small)
            .color(if copied { Color::Success } else { Color::Muted }),
        )
        .tooltip({
            tooltip_with_prefix(
                vec![(
                    format!("Copy {label}"),
                    identifier.to_string(),
                    Color::Accent,
                )],
                change_id_prefix,
            )
        })
        .on_click(move |_, _, cx| {
            cx.stop_propagation();
            cx.write_to_clipboard(ClipboardItem::new_string(identifier.to_string()));
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kk_change_identifier_highlights_only_verified_prefixes() {
        for prefix in ["k", "kkkkkkkk", "kkkkkkkkkl"] {
            let label = change_identifier("kkkkkkkkklmnop", prefix);
            assert_eq!(
                label.highlight_indices(),
                &(0..prefix.len()).collect::<Vec<_>>()
            );
        }
        for identifier in ["kkkkkkkkkl", "kkkkkkkkm"] {
            let label = change_identifier(identifier, identifier);
            assert_eq!(label.highlight_indices().len(), identifier.len());
        }
        for prefix in ["", "other", "kkkkkkkkklmnopq"] {
            assert!(
                change_identifier("kkkkkkkkklmnop", prefix)
                    .highlight_indices()
                    .is_empty()
            );
        }
    }
}
