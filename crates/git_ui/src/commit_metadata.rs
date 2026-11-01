use gpui::{AnyView, App, Window};
use ui::{Tooltip, prelude::*};

pub(crate) fn identifier(value: impl Into<gpui::SharedString>) -> Label {
    Label::new(value).color(Color::Accent)
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
                    .child(Label::new(value.clone()).color(*color))
            }))
            .into_any_element()
    })
}
