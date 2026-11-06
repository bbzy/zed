use gpui::{AnyView, App, Window};
use ui::{HighlightedLabel, Tooltip, prelude::*};

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
