use gpui::{App, Context, DismissEvent, Task, Window};
use kk_workbench::{Revision, Snapshot};
use picker::{Picker, PickerDelegate};
use std::{path::PathBuf, sync::Arc};
use ui::{ListItem, ListItemSpacing, prelude::*};
use util::ResultExt as _;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    Compare,
    Find,
}

type Confirm = Box<dyn Fn(PathBuf, Revision, &mut Window, &mut App)>;

pub(super) fn new(
    snapshot: Snapshot,
    purpose: Purpose,
    window: &mut Window,
    cx: &mut Context<Picker<RevisionDelegate>>,
    confirm: impl Fn(PathBuf, Revision, &mut Window, &mut App) + 'static,
) -> Picker<RevisionDelegate> {
    Picker::uniform_list(
        RevisionDelegate {
            snapshot,
            purpose,
            confirm: Box::new(confirm),
            matches: Vec::new(),
            selected_index: 0,
            generation: 0,
            query: String::new(),
            error: None,
            loading: false,
        },
        window,
        cx,
    )
    .initial_width(rems(42.))
    .show_scrollbar(true)
}

pub(super) struct RevisionDelegate {
    snapshot: Snapshot,
    purpose: Purpose,
    confirm: Confirm,
    matches: Vec<Revision>,
    selected_index: usize,
    generation: u64,
    query: String,
    error: Option<String>,
    loading: bool,
}

impl PickerDelegate for RevisionDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "KK revision picker"
    }

    fn placeholder_text(&self, _: &mut Window, _: &mut App) -> Arc<str> {
        match self.purpose {
            Purpose::Compare => "Compare with change ID, commit ID, message, bookmark, or tag…",
            Purpose::Find => "Find in ..@ by change ID, commit ID, message, bookmark, or tag…",
        }
        .into()
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(&mut self, index: usize, _: &mut Window, _: &mut Context<Picker<Self>>) {
        self.selected_index = index;
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        self.generation += 1;
        let generation = self.generation;
        let snapshot = self.snapshot.clone();
        let purpose = self.purpose;
        let selected = self.matches.get(self.selected_index).cloned();
        if self.query != query {
            self.matches.clear();
            self.selected_index = 0;
        }
        self.query = query.clone();
        self.error = None;
        let read = if query.trim().is_empty() {
            match purpose {
                Purpose::Compare => kk_workbench::remote::Read::ComparisonCandidates,
                Purpose::Find => kk_workbench::remote::Read::Revisions {
                    revset: Some("..@".into()),
                    limit: 20,
                },
            }
        } else {
            kk_workbench::remote::Read::SearchRevisions {
                query: query.clone(),
                scope: (purpose == Purpose::Find).then(|| "..@".into()),
                working_copy: purpose == Purpose::Find,
                limit: if purpose == Purpose::Find { 20 } else { 50 },
            }
        };
        if let Some(matches) = snapshot.cached::<Vec<Revision>>(&read) {
            self.selected_index = selected
                .as_ref()
                .and_then(|selected| selection_index(&matches, selected))
                .unwrap_or(0);
            self.matches = matches;
            self.loading = false;
            cx.notify();
            return Task::ready(());
        }
        if let Some((_, matches)) = snapshot.previously_cached::<Vec<Revision>>(&read) {
            self.selected_index = selected
                .as_ref()
                .and_then(|selected| selection_index(&matches, selected))
                .unwrap_or(0);
            self.matches = matches;
        }
        self.loading = true;
        cx.notify();
        cx.spawn_in(window, async move |picker, cx| {
            let result = cx
                .background_spawn(async move {
                    let revisions = if query.trim().is_empty() {
                        match purpose {
                            Purpose::Compare => snapshot.comparison_candidates().await,
                            Purpose::Find => snapshot.revisions(Some("..@"), 20).await,
                        }
                    } else {
                        snapshot
                            .search_revisions(
                                &query,
                                (purpose == Purpose::Find).then_some("..@"),
                                purpose == Purpose::Find,
                                if purpose == Purpose::Find { 20 } else { 50 },
                            )
                            .await
                    };
                    match revisions {
                        Ok(revisions) => (revisions, None),
                        Err(error) => (
                            Vec::new(),
                            Some(format!(
                                "{}: {error:#}",
                                snapshot.repository().root().display()
                            )),
                        ),
                    }
                })
                .await;
            picker
                .update(cx, |picker, cx| {
                    let delegate = &mut picker.delegate;
                    if delegate.generation != generation {
                        return;
                    }

                    let (matches, error) = result;
                    if error.is_some() {
                        delegate.error = error;
                        delegate.loading = false;
                        cx.notify();
                        return;
                    }
                    let selected = delegate
                        .matches
                        .get(delegate.selected_index)
                        .cloned()
                        .or(selected);
                    delegate.selected_index = selected
                        .as_ref()
                        .and_then(|selected| selection_index(&matches, selected))
                        .unwrap_or(0);
                    delegate.matches = matches;
                    delegate.error = error;
                    delegate.loading = false;
                    cx.notify();
                })
                .log_err();
        })
    }

    fn confirm(&mut self, _: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        if let Some(revision) = self.matches.get(self.selected_index) {
            (self.confirm)(
                self.snapshot.repository().root().to_owned(),
                revision.clone(),
                window,
                cx,
            );
            cx.emit(DismissEvent);
        }
    }

    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        cx.emit(DismissEvent);
    }

    fn render_match(
        &self,
        index: usize,
        selected: bool,
        _: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let revision = self.matches.get(index)?;
        let references = revision
            .bookmarks
            .iter()
            .cloned()
            .chain(revision.remote_bookmarks())
            .chain(revision.tags.iter().cloned())
            .collect::<Vec<_>>()
            .join(", ");
        let query = self.query.trim().to_lowercase();
        let body_match = (!query.is_empty())
            .then(|| {
                revision
                    .full_description
                    .lines()
                    .skip(1)
                    .find(|line| line.to_lowercase().contains(&query))
            })
            .flatten();
        Some(
            ListItem::new(index)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(
                    v_flex()
                        .min_w_0()
                        .child(
                            h_flex()
                                .gap_1()
                                .min_w_0()
                                .child(super::commit_metadata::change_identifier(
                                    revision.change_id.clone(),
                                    &revision.change_id_prefix,
                                ))
                                .child(
                                    super::commit_metadata::description(
                                        revision.subject().to_owned(),
                                    )
                                    .truncate(),
                                ),
                        )
                        .child(
                            h_flex()
                                .gap_1()
                                .min_w_0()
                                .child(
                                    super::commit_metadata::person(format!(
                                        "{} <{}>",
                                        revision.author_name, revision.author_email
                                    ))
                                    .size(LabelSize::Small),
                                )
                                .child(
                                    super::commit_metadata::identifier(revision.commit_id.clone())
                                        .size(LabelSize::Small),
                                )
                                .child(
                                    super::commit_metadata::identifier(references)
                                        .size(LabelSize::Small)
                                        .truncate(),
                                ),
                        )
                        .when_some(body_match, |view, text| {
                            view.child(
                                Label::new(text.to_owned())
                                    .size(LabelSize::Small)
                                    .truncate(),
                            )
                        }),
                ),
        )
    }

    fn render_footer(&self, _: &mut Window, _: &mut Context<Picker<Self>>) -> Option<AnyElement> {
        Some(
            div()
                .p_2()
                .child(
                    Label::new(self.error.clone().unwrap_or_else(|| {
                        if self.loading {
                            "Searching…".into()
                        } else {
                            "Results are ranked by match quality; IDs match from the start.".into()
                        }
                    }))
                    .size(LabelSize::Small)
                    .color(if self.error.is_some() {
                        Color::Error
                    } else {
                        Color::Muted
                    }),
                )
                .into_any_element(),
        )
    }
}

fn selection_index(matches: &[Revision], selected: &Revision) -> Option<usize> {
    matches
        .iter()
        .position(|revision| revision.full_commit_id == selected.full_commit_id)
        .or_else(|| {
            let mut matching_changes = matches
                .iter()
                .enumerate()
                .filter(|(_, revision)| revision.full_change_id == selected.full_change_id);
            let (index, _) = matching_changes.next()?;
            matching_changes.next().is_none().then_some(index)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test(iterations = 3)]
    async fn kk_cache_picker_shows_matches_before_any_background_work(cx: &mut TestAppContext) {
        let directory = crate::kk_multi_diff::tests::local_fixture(cx).await;
        let root = directory.path().join("repo");
        let (snapshot, query) = cx
            .background_spawn(async move {
                let snapshot = kk_workbench::Repository::open(&root, std::path::Path::new("kk"))
                    .await?
                    .snapshot(true)
                    .await?;
                let candidates = snapshot.comparison_candidates().await?;
                let query = candidates
                    .first()
                    .expect("comparison candidate")
                    .change_id
                    .clone();
                snapshot.search_revisions(&query, None, false, 50).await?;
                snapshot
                    .search_revisions("missing-query", None, false, 50)
                    .await?;
                anyhow::Ok((snapshot, query))
            })
            .await
            .expect("cached candidates");
        cx.update(editor::init);
        let cx = cx.add_empty_window();
        let picker = cx.new_window_entity(|window, cx| {
            new(snapshot, Purpose::Compare, window, cx, |_, _, _, _| {})
        });
        for query in ["", query.as_str(), "missing-query", ""] {
            picker.update_in(cx, |picker, window, cx| {
                let task = picker.delegate.update_matches(query.into(), window, cx);
                assert!(!picker.delegate.loading);
                assert_eq!(picker.delegate.matches.is_empty(), query == "missing-query");
                task.detach();
            });
        }
    }
}
