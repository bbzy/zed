use serde::{Deserialize, Serialize};
use std::collections::HashSet;

pub const HISTORY_PAGE_SIZE: usize = 50;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub enum HistoryMode {
    #[default]
    Lineage,
    Log,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Revision {
    pub change_id: String,
    pub full_change_id: String,
    pub commit_id: String,
    pub full_commit_id: String,
    pub description: String,
    pub full_description: String,
    pub author_name: String,
    pub author_email: String,
    pub committer_name: String,
    pub committer_email: String,
    #[serde(default)]
    pub commit_timestamp: Option<i64>,
    pub bookmarks: Vec<String>,
    pub remote_bookmark_names: Vec<String>,
    pub remote_bookmark_remotes: Vec<String>,
    pub tags: Vec<String>,
    pub parent_change_ids: Vec<String>,
    pub parent_commit_ids: Vec<String>,
    pub has_multiple_children: bool,
    pub has_conflict: bool,
    pub is_working_copy: bool,
    pub is_root: bool,
}

impl Revision {
    pub fn revset(&self) -> String {
        format!("change_id({})", self.full_change_id)
    }

    pub fn subject(&self) -> &str {
        if self.description.is_empty() {
            "(no description set)"
        } else {
            &self.description
        }
    }

    pub fn remote_bookmarks(&self) -> impl Iterator<Item = String> + '_ {
        self.remote_bookmark_names
            .iter()
            .enumerate()
            .map(|(index, name)| {
                match self
                    .remote_bookmark_remotes
                    .get(index)
                    .filter(|remote| !remote.is_empty())
                {
                    Some(remote) => format!("{name}@{remote}"),
                    None => name.clone(),
                }
            })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HistoryPage {
    pub revisions: Vec<Revision>,
    pub has_more: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphRow {
    pub prefix: String,
    pub column: usize,
    pub incoming: Vec<usize>,
    pub outgoing: Vec<usize>,
    pub parents: Vec<usize>,
    pub is_fork: bool,
    pub is_merge: bool,
    pub omitted_parents: usize,
}

pub fn rank_revisions(revisions: Vec<Revision>, query: &str) -> Vec<Revision> {
    let query = query.trim();
    if query.is_empty() {
        return revisions;
    }

    let folded = query.to_lowercase();
    let mut matches: Vec<_> = revisions
        .into_iter()
        .filter_map(|revision| {
            let mut scores = Vec::new();
            for (identifier, bonus) in [
                (&revision.change_id, 200),
                (&revision.full_change_id, 200),
                (&revision.commit_id, 100),
                (&revision.full_commit_id, 100),
            ] {
                if identifier.starts_with(query) {
                    scores.push(if identifier == query {
                        9000 + bonus
                    } else {
                        4000 + query.len().min(12) * 300
                            + bonus
                            + 9usize.saturating_sub(identifier.len().saturating_sub(query.len()))
                    });
                }
            }

            let references: Vec<_> = revision.remote_bookmarks().collect();
            let fields = [
                (&revision.description, 10),
                (&revision.full_description, 10),
            ]
            .into_iter()
            .chain(
                revision
                    .bookmarks
                    .iter()
                    .chain(references.iter())
                    .chain(revision.tags.iter())
                    .map(|reference| (reference, 30)),
            );
            for (value, bonus) in fields {
                let value = value.to_lowercase();
                if let Some(index) = value.find(&folded) {
                    let score = if value == folded {
                        7000 + bonus
                    } else if index == 0 {
                        5000 + bonus
                            + 9usize.saturating_sub(
                                value
                                    .encode_utf16()
                                    .count()
                                    .saturating_sub(folded.encode_utf16().count()),
                            )
                    } else {
                        let boundary = value[..index]
                            .chars()
                            .next_back()
                            .is_some_and(|character| !character.is_alphanumeric());
                        (if boundary { 3000 } else { 1000 })
                            + bonus
                            + 9usize.saturating_sub(value[..index].encode_utf16().count())
                    };
                    scores.push(score);
                }
            }

            scores.into_iter().max().map(|score| (score, revision))
        })
        .collect();
    matches.sort_by_key(|(score, _)| std::cmp::Reverse(*score));

    matches.into_iter().map(|(_, revision)| revision).collect()
}

pub fn history_graph(revisions: &[Revision], mode: HistoryMode) -> Vec<GraphRow> {
    let visible: HashSet<&str> = revisions
        .iter()
        .map(|revision| revision.full_commit_id.as_str())
        .collect();
    let mut lanes: Vec<Option<&str>> = Vec::new();
    let mut rows = Vec::with_capacity(revisions.len());
    for revision in revisions {
        let node = if revision.is_working_copy {
            '@'
        } else if revision.has_conflict {
            '×'
        } else {
            '○'
        };
        if mode == HistoryMode::Lineage {
            rows.push(GraphRow {
                prefix: node.to_string(),
                column: 0,
                incoming: Vec::new(),
                outgoing: Vec::new(),
                parents: Vec::new(),
                is_fork: false,
                is_merge: false,
                omitted_parents: revision
                    .parent_commit_ids
                    .first()
                    .map_or(0, |parent| usize::from(!visible.contains(parent.as_str()))),
            });
            continue;
        }

        let column = lanes
            .iter()
            .position(|lane| *lane == Some(revision.full_commit_id.as_str()))
            .or_else(|| lanes.iter().position(Option::is_none))
            .unwrap_or(lanes.len());
        if column == lanes.len() {
            lanes.push(None);
        }

        let has_incoming = lanes[column].is_some();
        lanes[column] = Some(&revision.full_commit_id);
        let incoming = lanes.clone();
        lanes[column] = None;
        let mut outgoing = Vec::new();
        let mut parents = HashSet::new();
        let mut omitted_parents = 0;
        for parent in &revision.parent_commit_ids {
            if !parents.insert(parent.as_str()) {
                continue;
            }

            if !visible.contains(parent.as_str()) {
                omitted_parents += 1;
                continue;
            }

            let target = lanes
                .iter()
                .position(|lane| *lane == Some(parent.as_str()))
                .unwrap_or_else(|| {
                    if outgoing.is_empty() && lanes[column].is_none() {
                        column
                    } else {
                        lanes
                            .iter()
                            .position(Option::is_none)
                            .unwrap_or(lanes.len())
                    }
                });
            if target == lanes.len() {
                lanes.push(None);
            }

            lanes[target] = Some(parent);
            outgoing.push(target);
        }

        let width = incoming.len().max(lanes.len());
        let mut cells: Vec<char> = (0..width * 2 - 1)
            .map(|index| {
                if index % 2 == 0
                    && (incoming.get(index / 2).is_some_and(Option::is_some)
                        || lanes.get(index / 2).is_some_and(Option::is_some))
                {
                    '│'
                } else {
                    ' '
                }
            })
            .collect();
        for target in outgoing.iter().copied() {
            for cell in cells
                .iter_mut()
                .take(column.max(target) * 2)
                .skip(column.min(target) * 2 + 1)
            {
                *cell = '─';
            }
        }

        for lane in 0..width {
            let from_above = incoming.get(lane).is_some_and(Option::is_some);
            if !from_above && !lanes.get(lane).is_some_and(Option::is_some) {
                continue;
            }

            let left = lane
                .checked_mul(2)
                .and_then(|index| index.checked_sub(1))
                .and_then(|index| cells.get(index))
                == Some(&'─');
            let right = cells.get(lane * 2 + 1) == Some(&'─');
            cells[lane * 2] = match (left, right, from_above) {
                (true, true, true) => '┼',
                (true, true, false) => '┬',
                (true, false, true) => '┤',
                (true, false, false) => '╮',
                (false, true, true) => '├',
                (false, true, false) => '╭',
                _ => cells[lane * 2],
            };
        }

        cells[column * 2] = node;
        let mut prefix = cells.into_iter().collect::<String>().trim_end().to_owned();
        if omitted_parents > 0 {
            prefix.push_str(" ~");
        }

        rows.push(GraphRow {
            prefix,
            column,
            incoming: incoming
                .iter()
                .enumerate()
                .filter_map(|(index, lane)| {
                    lane.filter(|_| index != column || has_incoming)
                        .map(|_| index)
                })
                .collect(),
            outgoing: lanes
                .iter()
                .enumerate()
                .filter_map(|(index, lane)| lane.map(|_| index))
                .collect(),
            parents: outgoing,
            is_fork: revision.has_multiple_children,
            is_merge: revision.parent_commit_ids.len() > 1,
            omitted_parents,
        });
        while lanes.last() == Some(&None) {
            lanes.pop();
        }
    }

    rows
}
