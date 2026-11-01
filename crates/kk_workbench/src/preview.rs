use crate::{
    LineAnchor, LineSelection, SelectionProjection, SelectionSide,
    selection::{apply_atoms, local_line_edits, positional_atoms, select_atoms, text_lines},
};
use anyhow::{Context as _, Result, ensure};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferDirection {
    Backward,
    Forward,
}

#[derive(Clone)]
struct CompletionGroup {
    required: Vec<LineAnchor>,
    extra: Vec<LineAnchor>,
}

#[derive(Clone)]
pub struct ClassificationPreview {
    confirmed: String,
    head: String,
    shown: String,
    shown_modified: String,
    pending: Vec<(u64, LineSelection, TransferDirection)>,
    snapshots: HashMap<String, Vec<LineAnchor>>,
    modified_snapshots: HashMap<String, Vec<LineAnchor>>,
    edits: Option<Vec<crate::LineEdit>>,
    completion_groups: Vec<CompletionGroup>,
}

impl ClassificationPreview {
    pub fn new(original: &str, modified: &str) -> Result<Self> {
        let mut preview = Self::for_whole_file(original, modified);
        preview.edits = Some(
            local_line_edits(&preview.confirmed, &preview.head)
                .context("This diff is too large to preview locally")?,
        );
        Ok(preview)
    }

    pub fn for_whole_file(original: &str, modified: &str) -> Self {
        let confirmed = original.replace("\r\n", "\n");
        let head = modified.replace("\r\n", "\n");
        Self {
            shown: confirmed.clone(),
            shown_modified: head.clone(),
            snapshots: HashMap::from([(confirmed.clone(), original_lines(&confirmed))]),
            modified_snapshots: HashMap::from([(head.clone(), modified_lines(&head))]),
            confirmed,
            head,
            pending: Vec::new(),
            edits: None,
            completion_groups: Vec::new(),
        }
    }

    pub fn text(&self) -> &str {
        &self.shown
    }
    pub fn modified_text(&self) -> &str {
        &self.shown_modified
    }
    pub fn confirmed(&self) -> &str {
        &self.confirmed
    }
    pub fn head(&self) -> &str {
        &self.head
    }
    pub fn is_pending(&self) -> bool {
        !self.pending.is_empty()
    }
    pub fn pending_ids(&self) -> impl Iterator<Item = u64> + '_ {
        self.pending.iter().map(|(id, _, _)| *id)
    }
    pub fn knows(&self, original: &str, modified: &str) -> bool {
        self.snapshots.contains_key(&original.replace("\r\n", "\n"))
            && self
                .modified_snapshots
                .contains_key(&modified.replace("\r\n", "\n"))
    }

    pub fn capture(
        &mut self,
        original: &str,
        modified: &str,
        selection: &LineSelection,
    ) -> Result<LineSelection> {
        let original = original.replace("\r\n", "\n");
        let modified = modified.replace("\r\n", "\n");
        let original_lines = self.snapshots.get(&original).context(
            "The diff changed while classification was pending. Select the updated changes.",
        )?;
        let modified_lines = self.modified_snapshots.get(&modified).context(
            "The file changed while classification was pending. Select the updated changes.",
        )?;
        if selection.whole_file {
            return Ok(selection.clone());
        }

        let current = original == self.confirmed && modified == self.head;
        let edits = if current {
            self.edits
                .clone()
                .or_else(|| local_line_edits(&original, &modified))
        } else {
            local_line_edits(&original, &modified)
        }
        .context("This diff is too large to preview locally")?;
        if current {
            self.edits = Some(edits.clone());
        }

        let selected = select_atoms(
            selection,
            &edits,
            &positional_atoms(&edits),
            original_lines.len(),
            modified_lines.len(),
        )?;
        ensure!(
            !selected.is_empty(),
            "The selection does not contain any changed lines"
        );
        for edit in &edits {
            let (required, extra) = match selection.side {
                SelectionSide::Original => (
                    original_lines.get(edit.original.clone()),
                    modified_lines.get(edit.modified.clone()),
                ),
                SelectionSide::Modified => (
                    modified_lines.get(edit.modified.clone()),
                    original_lines.get(edit.original.clone()),
                ),
            };
            let required = required.context("Invalid selected-side range")?;
            let extra = extra.context("Invalid opposite-side range")?;
            if !required.is_empty() && extra.len() > required.len() {
                self.completion_groups.push(CompletionGroup {
                    required: required.to_vec(),
                    extra: required.iter().chain(extra).copied().collect(),
                });
            }
        }

        let mut anchors = Vec::new();
        for atom in selected {
            let include_original = selection.cursor.is_some()
                || selection.side == SelectionSide::Original
                || atom.modified.is_none();
            let include_modified = selection.cursor.is_some()
                || selection.side == SelectionSide::Modified
                || atom.original.is_none();
            if include_original && let Some(index) = atom.original {
                anchors.push(
                    *original_lines
                        .get(index)
                        .context("Invalid original anchor")?,
                );
            }

            if include_modified && let Some(index) = atom.modified {
                anchors.push(
                    *modified_lines
                        .get(index)
                        .context("Invalid modified anchor")?,
                );
            }
        }

        Ok(LineSelection::anchored(selection.side, anchors))
    }

    pub fn add(
        &mut self,
        id: u64,
        selection: LineSelection,
        direction: TransferDirection,
    ) -> Result<()> {
        if let Some(entry) = self
            .pending
            .iter_mut()
            .find(|(pending, _, _)| *pending == id)
        {
            *entry = (id, selection, direction);
        } else {
            self.pending.push((id, selection, direction));
        }

        self.render()
    }

    pub fn remove(&mut self, id: u64) -> Result<()> {
        self.pending.retain(|(pending, _, _)| *pending != id);
        self.render()
    }

    pub fn selection(&self, id: u64) -> Result<Option<LineSelection>> {
        Ok(self
            .effective_selections()?
            .into_iter()
            .find(|(pending, _, _)| *pending == id)
            .map(|(_, selection, _)| selection))
    }

    pub fn native_ranges(&self, id: u64) -> Result<Vec<crate::LineRange>> {
        let edits = self
            .edits
            .clone()
            .or_else(|| local_line_edits(&self.confirmed, &self.head))
            .context("This diff is too large to preview locally")?;
        let selection = self
            .selection(id)?
            .context("Missing pending classification")?;
        let projection = SelectionProjection {
            atoms: positional_atoms(&edits),
            edits,
            original_ids: (0..text_lines(&self.confirmed).len()).collect(),
            modified_ids: (0..text_lines(&self.head).len()).collect(),
            destination_ids: Vec::new(),
            modified_destination_ids: None,
            remaining_edits: Vec::new(),
        };
        crate::native_selection_ranges(&selection, &projection)
    }

    pub fn reconcile_external(&mut self, original: &str, modified: &str) -> Vec<u64> {
        let cancelled = self.pending_ids().collect();
        *self = Self::for_whole_file(original, modified);
        self.edits = local_line_edits(&self.confirmed, &self.head);
        cancelled
    }

    pub fn accept(
        &mut self,
        original: &str,
        modified: &str,
        projection: &SelectionProjection,
        completed: &[u64],
    ) -> Result<()> {
        let selections = self.effective_selections()?;
        let selections: Vec<_> = selections
            .into_iter()
            .filter(|(id, _, _)| completed.contains(id))
            .map(|(_, selection, _)| selection)
            .collect();
        let whole_file = selections.iter().any(|selection| selection.whole_file);
        let anchors: Vec<_> = selections
            .iter()
            .flat_map(|selection| selection.anchors.as_deref().unwrap_or_default())
            .copied()
            .collect();
        self.pending.retain(|(id, _, _)| !completed.contains(id));
        let original_indices: HashMap<_, _> = projection
            .destination_ids
            .iter()
            .enumerate()
            .map(|(index, id)| (*id, index))
            .collect();
        let modified_ids = projection
            .modified_destination_ids
            .as_deref()
            .unwrap_or(&projection.modified_ids);
        let modified_indices: HashMap<_, _> = modified_ids
            .iter()
            .enumerate()
            .map(|(index, id)| (*id, index))
            .collect();
        let advance = |anchor: &LineAnchor| LineAnchor {
            original: anchor
                .original
                .and_then(|index| projection.original_ids.get(index))
                .and_then(|id| original_indices.get(id))
                .copied(),
            modified: anchor
                .modified
                .and_then(|index| projection.modified_ids.get(index))
                .and_then(|id| modified_indices.get(id))
                .copied(),
        };
        for (_, selection, _) in &mut self.pending {
            if let Some(anchors) = &mut selection.anchors {
                *anchors = anchors.iter().map(advance).collect();
            }
        }

        self.completion_groups = self
            .completion_groups
            .iter()
            .filter_map(|group| {
                let required: Vec<_> = group
                    .required
                    .iter()
                    .filter(|anchor| {
                        !whole_file && !anchors.iter().any(|done| same_anchor(anchor, done))
                    })
                    .map(advance)
                    .collect();
                (!required.is_empty()).then(|| CompletionGroup {
                    required,
                    extra: group.extra.iter().map(advance).collect(),
                })
            })
            .collect();
        for lines in self
            .snapshots
            .values_mut()
            .chain(self.modified_snapshots.values_mut())
        {
            for anchor in lines {
                let mut next = advance(anchor);
                next.original = next.original.or_else(|| {
                    anchor
                        .modified
                        .and_then(|index| projection.modified_ids.get(index))
                        .and_then(|id| original_indices.get(id))
                        .copied()
                });
                *anchor = next;
            }
        }

        self.confirmed = original.replace("\r\n", "\n");
        self.head = modified.replace("\r\n", "\n");
        self.edits = Some(
            local_line_edits(&self.confirmed, &self.head)
                .unwrap_or_else(|| projection.remaining_edits.clone()),
        );
        self.snapshots
            .insert(self.confirmed.clone(), original_lines(&self.confirmed));
        self.modified_snapshots
            .insert(self.head.clone(), modified_lines(&self.head));
        self.render()
    }

    fn effective_selections(&self) -> Result<Vec<(u64, LineSelection, TransferDirection)>> {
        let Some(edits) = self
            .edits
            .clone()
            .or_else(|| local_line_edits(&self.confirmed, &self.head))
        else {
            return Ok(self
                .pending
                .iter()
                .map(|(id, selection, direction)| (*id, selection.clone(), *direction))
                .collect());
        };
        let candidates = positional_atoms(&edits);
        let resolve = |selection: &LineSelection| {
            select_atoms(
                selection,
                &edits,
                &candidates,
                text_lines(&self.confirmed).len(),
                text_lines(&self.head).len(),
            )
        };
        let mut covered = HashSet::new();
        let mut coverage = Vec::new();
        let mut result = Vec::new();
        for (id, selection, direction) in &self.pending {
            let mut selected = resolve(selection)?;
            coverage.extend_from_slice(selection.anchors.as_deref().unwrap_or_default());
            covered.extend(selected.iter().copied());
            for group in &self.completion_groups {
                if group.required.is_empty()
                    || !group
                        .required
                        .iter()
                        .all(|anchor| coverage.iter().any(|chosen| same_anchor(anchor, chosen)))
                {
                    continue;
                }

                for atom in resolve(&LineSelection::anchored(
                    selection.side,
                    group.extra.clone(),
                ))? {
                    if covered.insert(atom) {
                        selected.push(atom);
                    }
                }
            }

            let effective = if selection.whole_file {
                selection.clone()
            } else {
                LineSelection::anchored(
                    selection.side,
                    selected
                        .iter()
                        .map(|atom| LineAnchor {
                            original: atom.original,
                            modified: atom.modified,
                        })
                        .collect(),
                )
            };
            result.push((*id, effective, *direction));
        }

        Ok(result)
    }

    fn render(&mut self) -> Result<()> {
        let original = text_lines(&self.confirmed);
        let modified = text_lines(&self.head);
        self.shown_modified.clone_from(&self.head);
        let forward = self
            .pending
            .iter()
            .any(|(_, _, direction)| *direction == TransferDirection::Forward);
        let lines = if forward
            && self.pending.len() == 1
            && self
                .pending
                .first()
                .is_some_and(|(_, selection, _)| selection.whole_file)
        {
            self.shown.clone_from(&self.confirmed);
            self.shown_modified.clone_from(&self.confirmed);
            self.modified_snapshots
                .insert(self.shown_modified.clone(), original_lines(&self.confirmed));
            original_lines(&self.confirmed)
        } else if !forward && self.pending.is_empty() {
            self.shown.clone_from(&self.confirmed);
            original_lines(&self.confirmed)
        } else if !forward
            && self
                .pending
                .iter()
                .any(|(_, selection, _)| selection.whole_file)
        {
            self.shown.clone_from(&self.head);
            modified_lines(&self.head)
        } else {
            let edits = self
                .edits
                .clone()
                .or_else(|| local_line_edits(&self.confirmed, &self.head))
                .context("This diff is too large to preview locally")?;
            self.edits = Some(edits.clone());
            let candidates = positional_atoms(&edits);
            let mut directions = HashMap::new();
            for (_, selection, direction) in self.effective_selections()? {
                for atom in select_atoms(
                    &selection,
                    &edits,
                    &candidates,
                    original.len(),
                    modified.len(),
                )? {
                    directions.entry(atom).or_insert(direction);
                }
            }

            let backward: Vec<_> = candidates
                .iter()
                .copied()
                .filter(|atom| directions.get(atom) == Some(&TransferDirection::Backward))
                .collect();
            let original_lines = original_lines(&self.confirmed);
            let modified_lines = modified_lines(&self.head);
            self.shown = apply_atoms(&original, &modified, &backward)?.concat();
            if forward {
                let remaining: Vec<_> = candidates
                    .iter()
                    .copied()
                    .filter(|atom| directions.get(atom) != Some(&TransferDirection::Forward))
                    .collect();
                self.shown_modified = apply_atoms(&original, &modified, &remaining)?.concat();
                self.modified_snapshots.insert(
                    self.shown_modified.clone(),
                    apply_atoms(&original_lines, &modified_lines, &remaining)?,
                );
            }

            apply_atoms(&original_lines, &modified_lines, &backward)?
        };
        self.snapshots.insert(self.shown.clone(), lines);
        Ok(())
    }
}

fn original_lines(text: &str) -> Vec<LineAnchor> {
    text_lines(text)
        .iter()
        .enumerate()
        .map(|(index, _)| LineAnchor {
            original: Some(index),
            modified: None,
        })
        .collect()
}
fn modified_lines(text: &str) -> Vec<LineAnchor> {
    text_lines(text)
        .iter()
        .enumerate()
        .map(|(index, _)| LineAnchor {
            original: None,
            modified: Some(index),
        })
        .collect()
}
fn same_anchor(left: &LineAnchor, right: &LineAnchor) -> bool {
    left.original.is_some() && left.original == right.original
        || left.modified.is_some() && left.modified == right.modified
}
