use crate::SelectionSide;
use anyhow::{Context as _, Result, ensure};
use std::{
    collections::{HashMap, HashSet},
    ops::Range,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineEdit {
    pub original: Range<usize>,
    pub modified: Range<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LineAtom {
    pub original: Option<usize>,
    pub modified: Option<usize>,
    pub gap: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct LineAnchor {
    pub original: Option<usize>,
    pub modified: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineSelection {
    pub side: SelectionSide,
    pub ranges: Vec<Range<usize>>,
    pub cursor: Option<usize>,
    pub whole_file: bool,
    pub anchors: Option<Vec<LineAnchor>>,
}

impl LineSelection {
    pub fn whole_file() -> Self {
        Self {
            side: SelectionSide::Modified,
            ranges: Vec::new(),
            cursor: None,
            whole_file: true,
            anchors: None,
        }
    }

    pub fn ranges(side: SelectionSide, ranges: Vec<Range<usize>>) -> Self {
        Self {
            side,
            ranges,
            cursor: None,
            whole_file: false,
            anchors: None,
        }
    }

    pub fn anchored(side: SelectionSide, anchors: Vec<LineAnchor>) -> Self {
        Self {
            side,
            ranges: Vec::new(),
            cursor: None,
            whole_file: false,
            anchors: Some(anchors),
        }
    }
}

#[derive(Clone, Debug)]
pub struct SelectionProjection {
    pub edits: Vec<LineEdit>,
    pub atoms: Vec<LineAtom>,
    pub original_ids: Vec<usize>,
    pub modified_ids: Vec<usize>,
    pub destination_ids: Vec<usize>,
    pub modified_destination_ids: Option<Vec<usize>>,
    pub remaining_edits: Vec<LineEdit>,
}

#[derive(Clone, Debug)]
pub struct FileStep {
    pub before: Option<String>,
    pub after: Option<String>,
    pub edits: Vec<LineEdit>,
    pub edits_to_head: Vec<LineEdit>,
}

#[derive(Default)]
pub struct SelectionPlanOptions<'a> {
    pub prior_steps: &'a [FileStep],
    pub baseline_index: Option<usize>,
}

#[derive(Clone, Debug)]
pub struct SelectionPlan {
    pub selected_base: Option<String>,
    pub steps: Vec<Option<String>>,
    pub prior_steps: Vec<Option<String>>,
    pub source_ids: Vec<Vec<usize>>,
    pub projection: SelectionProjection,
}

pub(crate) fn text_lines(text: &str) -> Vec<&str> {
    text.split_inclusive('\n').collect()
}

pub fn local_line_edits(before: &str, after: &str) -> Option<Vec<LineEdit>> {
    let original = text_lines(before);
    let modified = text_lines(after);
    let mut start = 0;
    let mut original_end = original.len();
    let mut modified_end = modified.len();
    while start < original_end && start < modified_end && original.get(start) == modified.get(start)
    {
        start += 1;
    }

    while original_end > start
        && modified_end > start
        && original.get(original_end - 1) == modified.get(modified_end - 1)
    {
        original_end -= 1;
        modified_end -= 1;
    }

    let original_count = isize::try_from(original_end - start).ok()?;
    let modified_count = isize::try_from(modified_end - start).ok()?;
    if original_count == 0 || modified_count == 0 {
        return Some(if original_count == modified_count {
            Vec::new()
        } else {
            vec![LineEdit {
                original: start..original_end,
                modified: start..modified_end,
            }]
        });
    }

    let mut trace = Vec::new();
    let mut frontier = HashMap::from([(1_isize, 0_isize)]);
    let mut work = 0;
    for distance in 0..=original_count + modified_count {
        trace.push(frontier.clone());
        for diagonal in (-distance..=distance).step_by(2) {
            work += 1;
            if work > 250_000 {
                return None;
            }

            let mut original_index = if diagonal == -distance
                || diagonal != distance
                    && frontier.get(&(diagonal - 1)).copied().unwrap_or(-1)
                        < frontier.get(&(diagonal + 1)).copied().unwrap_or(-1)
            {
                frontier.get(&(diagonal + 1)).copied().unwrap_or(0)
            } else {
                frontier.get(&(diagonal - 1)).copied().unwrap_or(0) + 1
            };
            let mut modified_index = original_index - diagonal;
            while original_index < original_count
                && modified_index < modified_count
                && original.get(start + usize::try_from(original_index).ok()?)
                    == modified.get(start + usize::try_from(modified_index).ok()?)
            {
                original_index += 1;
                modified_index += 1;
            }

            frontier.insert(diagonal, original_index);
            if original_index < original_count || modified_index < modified_count {
                continue;
            }

            let mut matches = Vec::new();
            for distance in (0..=distance).rev() {
                let frontier = trace.get(usize::try_from(distance).ok()?)?;
                let diagonal = original_index - modified_index;
                let previous = if diagonal == -distance
                    || diagonal != distance
                        && frontier.get(&(diagonal - 1)).copied().unwrap_or(-1)
                            < frontier.get(&(diagonal + 1)).copied().unwrap_or(-1)
                {
                    diagonal + 1
                } else {
                    diagonal - 1
                };
                let previous_original = frontier.get(&previous).copied().unwrap_or(0);
                let previous_modified = previous_original - previous;
                while original_index > previous_original && modified_index > previous_modified {
                    original_index -= 1;
                    modified_index -= 1;
                    matches.push((
                        start + usize::try_from(original_index).ok()?,
                        start + usize::try_from(modified_index).ok()?,
                    ));
                }

                original_index = previous_original;
                modified_index = previous_modified;
            }

            matches.reverse();
            matches.push((original_end, modified_end));
            let mut edits = Vec::new();
            let mut original_start = start;
            let mut modified_start = start;
            for (original_index, modified_index) in matches {
                if original_index > original_start || modified_index > modified_start {
                    edits.push(LineEdit {
                        original: original_start..original_index,
                        modified: modified_start..modified_index,
                    });
                }

                original_start = original_index + 1;
                modified_start = modified_index + 1;
            }

            return Some(edits);
        }
    }

    None
}

pub(crate) fn positional_atoms(edits: &[LineEdit]) -> Vec<LineAtom> {
    edits
        .iter()
        .flat_map(|edit| {
            (0..edit.original.len().max(edit.modified.len())).map(move |index| LineAtom {
                original: (index < edit.original.len()).then_some(edit.original.start + index),
                modified: (index < edit.modified.len()).then_some(edit.modified.start + index),
                gap: edit.original.start + index.min(edit.original.len()),
            })
        })
        .collect()
}

pub(crate) fn lineage_atoms(
    edits: &[LineEdit],
    before_ids: &[usize],
    after_ids: &[usize],
) -> Result<Vec<LineAtom>> {
    let mut atoms = Vec::new();
    for edit in edits {
        let original_ids = before_ids
            .get(edit.original.clone())
            .context("Invalid original line range")?;
        let modified_ids = after_ids
            .get(edit.modified.clone())
            .context("Invalid modified line range")?;
        let original_indices: HashMap<_, _> = original_ids
            .iter()
            .enumerate()
            .map(|(index, id)| (*id, edit.original.start + index))
            .collect();
        let mut paired = HashSet::new();
        let mut gaps = Vec::with_capacity(modified_ids.len());
        let mut next_gap = edit.original.end;
        for id in modified_ids.iter().rev() {
            next_gap = original_indices.get(id).copied().unwrap_or(next_gap);
            gaps.push(next_gap);
        }

        for ((index, id), gap) in modified_ids.iter().enumerate().zip(gaps.into_iter().rev()) {
            let original = original_indices.get(id).copied();
            if let Some(original) = original {
                paired.insert(original);
            }

            atoms.push(LineAtom {
                original,
                modified: Some(edit.modified.start + index),
                gap,
            });
        }

        for index in edit.original.clone() {
            if !paired.contains(&index) {
                atoms.push(LineAtom {
                    original: Some(index),
                    modified: None,
                    gap: index,
                });
            }
        }
    }

    Ok(atoms)
}

pub(crate) fn apply_atoms<T: Clone>(
    before: &[T],
    after: &[T],
    atoms: &[LineAtom],
) -> Result<Vec<T>> {
    let mut removals = HashSet::new();
    let mut insertions: HashMap<usize, Vec<T>> = HashMap::new();
    for atom in atoms {
        ensure!(atom.gap <= before.len(), "Invalid insertion anchor");
        if let Some(original) = atom.original {
            ensure!(original < before.len(), "Invalid original line anchor");
            removals.insert(original);
        }

        if let Some(modified) = atom.modified {
            insertions.entry(atom.gap).or_default().push(
                after
                    .get(modified)
                    .context("Invalid modified line anchor")?
                    .clone(),
            );
        }
    }

    let mut result = Vec::new();
    for (index, line) in before.iter().enumerate() {
        result.extend(insertions.remove(&index).unwrap_or_default());
        if !removals.contains(&index) {
            result.push(line.clone());
        }
    }

    result.extend(insertions.remove(&before.len()).unwrap_or_default());
    Ok(result)
}

pub(crate) fn select_atoms(
    selection: &LineSelection,
    edits: &[LineEdit],
    candidates: &[LineAtom],
    original_length: usize,
    modified_length: usize,
) -> Result<Vec<LineAtom>> {
    if selection.whole_file {
        return Ok(candidates.to_vec());
    }

    if let Some(anchors) = &selection.anchors {
        let original: HashSet<_> = anchors
            .iter()
            .filter_map(|anchor| anchor.original)
            .collect();
        let modified: HashSet<_> = anchors
            .iter()
            .filter_map(|anchor| anchor.modified)
            .collect();
        return Ok(candidates
            .iter()
            .copied()
            .filter(|atom| {
                atom.original.is_some_and(|index| original.contains(&index))
                    || atom.modified.is_some_and(|index| modified.contains(&index))
            })
            .collect());
    }

    let selected_hunk = if let Some(cursor) = selection.cursor {
        Some(
            edits
                .iter()
                .find(|edit| {
                    let (range, length) = match selection.side {
                        SelectionSide::Original => (&edit.original, original_length),
                        SelectionSide::Modified => (&edit.modified, modified_length),
                    };
                    if cursor == length && range.end == length {
                        true
                    } else if range.is_empty() {
                        cursor == range.start.min(length.saturating_sub(1))
                    } else {
                        range.contains(&cursor)
                    }
                })
                .context("Place the cursor in a diff hunk or select changed lines")?,
        )
    } else {
        None
    };
    let mut ranges = selection.ranges.clone();
    ranges.sort_by_key(|range| range.start);
    let mut selected = Vec::new();
    for atom in candidates {
        if let Some(hunk) = selected_hunk {
            if atom
                .original
                .is_some_and(|index| hunk.original.contains(&index))
                || atom
                    .modified
                    .is_some_and(|index| hunk.modified.contains(&index))
            {
                selected.push(*atom);
            }

            continue;
        }

        let index = match selection.side {
            SelectionSide::Original => atom.original,
            SelectionSide::Modified => atom.modified,
        };
        let index = match index {
            Some(index) => index,
            None => {
                let edit = edits
                    .iter()
                    .find(|edit| match atom.original {
                        Some(index) => edit.original.contains(&index),
                        None => atom
                            .modified
                            .is_some_and(|index| edit.modified.contains(&index)),
                    })
                    .context("A line anchor is outside the diff")?;
                let side = match selection.side {
                    SelectionSide::Original => &edit.original,
                    SelectionSide::Modified => &edit.modified,
                };
                if !side.is_empty() {
                    let mut covered = side.start;
                    for range in &ranges {
                        if range.start > covered {
                            break;
                        }

                        covered = covered.max(range.end);
                    }

                    if covered >= side.end {
                        selected.push(*atom);
                    }

                    continue;
                }

                side.start
            }
        };
        if ranges.iter().any(|range| range.contains(&index)) {
            selected.push(*atom);
        }
    }

    Ok(selected)
}

pub fn plan_selection(
    base: Option<&str>,
    head: Option<&str>,
    edits: &[LineEdit],
    selection: &LineSelection,
    steps: &[FileStep],
    options: SelectionPlanOptions<'_>,
) -> Result<SelectionPlan> {
    let original = text_lines(base.unwrap_or_default());
    let modified = text_lines(head.unwrap_or_default());
    let initial = options
        .prior_steps
        .first()
        .map(|step| text_lines(step.before.as_deref().unwrap_or_default()))
        .unwrap_or_else(|| original.clone());
    let mut next_id = initial.len();
    let mut line_ids: Vec<_> = (0..initial.len()).collect();
    let mut base_ids = line_ids.clone();
    let mut tracked = Vec::new();
    for (index, step) in options.prior_steps.iter().chain(steps).enumerate() {
        let before = text_lines(step.before.as_deref().unwrap_or_default());
        let after = text_lines(step.after.as_deref().unwrap_or_default());
        let mut after_ids = Vec::new();
        let mut offset = 0;
        for edit in &step.edits {
            after_ids.extend_from_slice(
                line_ids
                    .get(offset..edit.original.start)
                    .context("Invalid ordered file diff")?,
            );
            for index in 0..edit.modified.len() {
                if index < edit.original.len() {
                    after_ids.push(
                        *line_ids
                            .get(edit.original.start + index)
                            .context("Invalid replacement line")?,
                    );
                } else {
                    after_ids.push(next_id);
                    next_id += 1;
                }
            }

            offset = edit.original.end;
        }

        after_ids.extend_from_slice(line_ids.get(offset..).context("Invalid file diff end")?);
        ensure!(
            after_ids.len() == after.len()
                && apply_atoms(&before, &after, &positional_atoms(&step.edits))?.concat()
                    == step.after.as_deref().unwrap_or_default(),
            "The file diff changed while preparing classification. Reopen the diff."
        );
        line_ids = after_ids;
        if index + 1 == options.prior_steps.len() {
            base_ids.clone_from(&line_ids);
        }

        tracked.push((step, after, line_ids.clone()));
    }

    let candidates = lineage_atoms(edits, &base_ids, &line_ids)?;
    let source_ids = tracked
        .iter()
        .skip(options.prior_steps.len())
        .map(|(_, _, ids)| ids.clone())
        .collect();
    if selection.whole_file && options.prior_steps.is_empty() {
        return Ok(SelectionPlan {
            selected_base: head.map(str::to_owned),
            steps: steps.iter().map(|_| head.map(str::to_owned)).collect(),
            prior_steps: Vec::new(),
            source_ids,
            projection: SelectionProjection {
                edits: edits.to_vec(),
                atoms: candidates,
                original_ids: base_ids,
                destination_ids: line_ids.clone(),
                modified_ids: line_ids,
                modified_destination_ids: None,
                remaining_edits: Vec::new(),
            },
        });
    }

    let selected = select_atoms(
        selection,
        edits,
        &candidates,
        original.len(),
        modified.len(),
    )?;
    ensure!(
        !selected.is_empty()
            || selection.whole_file
            || options.baseline_index.is_some()
                && selection.anchors.as_ref().is_some_and(Vec::is_empty),
        "The selection does not contain any changed lines"
    );
    let mut selected_ids = HashSet::new();
    for atom in &selected {
        if let Some(index) = atom.original {
            selected_ids.insert(*base_ids.get(index).context("Invalid original identity")?);
        }

        if let Some(index) = atom.modified {
            selected_ids.insert(*line_ids.get(index).context("Invalid modified identity")?);
        }
    }

    let selected_text = apply_atoms(&original, &modified, &selected)?.concat();
    let mut projected = Vec::new();
    for (step, after, after_ids) in &tracked {
        ensure!(
            apply_atoms(after, &modified, &positional_atoms(&step.edits_to_head))?.concat()
                == head.unwrap_or_default(),
            "The source-to-head diff changed. Reopen the diff."
        );
        let changes: Vec<_> = lineage_atoms(&step.edits_to_head, after_ids, &line_ids)?
            .into_iter()
            .filter(|atom| {
                atom.original
                    .and_then(|index| after_ids.get(index))
                    .is_some_and(|id| selected_ids.contains(id))
                    || atom
                        .modified
                        .and_then(|index| line_ids.get(index))
                        .is_some_and(|id| selected_ids.contains(id))
            })
            .collect();
        let text = apply_atoms(after, &modified, &changes)?.concat();
        let exists =
            head.is_some() && (step.after.is_some() || selection.whole_file && base.is_none());
        let text = if text.is_empty() && !exists {
            None
        } else {
            Some(text)
        };
        projected.push((text, apply_atoms(after_ids, &line_ids, &changes)?));
    }

    let baseline = options
        .baseline_index
        .map(|index| {
            projected
                .get(options.prior_steps.len() + index)
                .context("Invalid category baseline")
        })
        .transpose()?;
    let selected_base = if let Some((text, _)) = baseline {
        text.clone()
    } else if head.is_none() && selected_text.is_empty() {
        None
    } else {
        Some(selected_text)
    };
    let destination_ids = match baseline {
        Some((_, ids)) => ids.clone(),
        None => apply_atoms(&base_ids, &line_ids, &selected)?,
    };
    Ok(SelectionPlan {
        selected_base,
        steps: projected
            .iter()
            .skip(options.prior_steps.len())
            .enumerate()
            .map(|(index, (text, _))| {
                if selection.whole_file
                    && options
                        .baseline_index
                        .is_none_or(|baseline| index > baseline)
                {
                    head.map(str::to_owned)
                } else {
                    text.clone()
                }
            })
            .collect(),
        prior_steps: projected
            .iter()
            .take(options.prior_steps.len())
            .map(|(text, _)| text.clone())
            .collect(),
        source_ids,
        projection: SelectionProjection {
            edits: edits.to_vec(),
            atoms: candidates,
            original_ids: base_ids,
            destination_ids,
            modified_ids: line_ids,
            modified_destination_ids: None,
            remaining_edits: Vec::new(),
        },
    })
}

pub fn rebase_selection(
    selection: &LineSelection,
    projection: &SelectionProjection,
) -> Result<LineSelection> {
    if selection.whole_file {
        return Ok(selection.clone());
    }

    let selected = select_atoms(
        selection,
        &projection.edits,
        &projection.atoms,
        projection.original_ids.len(),
        projection.modified_ids.len(),
    )?;
    let destination: HashMap<_, _> = projection
        .destination_ids
        .iter()
        .enumerate()
        .map(|(index, id)| (*id, index))
        .collect();
    let modified_ids = projection
        .modified_destination_ids
        .as_deref()
        .unwrap_or(&projection.modified_ids);
    let modified: HashMap<_, _> = modified_ids
        .iter()
        .enumerate()
        .map(|(index, id)| (*id, index))
        .collect();
    let anchors = selected
        .iter()
        .map(|atom| LineAnchor {
            original: atom
                .original
                .and_then(|index| projection.original_ids.get(index))
                .and_then(|id| destination.get(id))
                .copied(),
            modified: atom
                .modified
                .and_then(|index| projection.modified_ids.get(index))
                .and_then(|id| modified.get(id))
                .copied(),
        })
        .collect();
    let remaining = lineage_atoms(
        &projection.remaining_edits,
        &projection.destination_ids,
        modified_ids,
    )?;
    let selection = LineSelection::anchored(selection.side, anchors);
    let anchors = select_atoms(&selection, &[], &remaining, 0, 0)?
        .into_iter()
        .map(|atom| LineAnchor {
            original: atom.original,
            modified: atom.modified,
        })
        .collect();
    Ok(LineSelection::anchored(selection.side, anchors))
}

pub fn native_selection_ranges(
    selection: &LineSelection,
    projection: &SelectionProjection,
) -> Result<Vec<crate::LineRange>> {
    let desired = select_atoms(
        selection,
        &projection.edits,
        &projection.atoms,
        projection.original_ids.len(),
        projection.modified_ids.len(),
    )?;
    if desired.is_empty() {
        return Ok(Vec::new());
    }

    let opposite = match selection.side {
        SelectionSide::Original => SelectionSide::Modified,
        SelectionSide::Modified => SelectionSide::Original,
    };
    for side in [selection.side, opposite] {
        let mut indices: Vec<_> = desired
            .iter()
            .filter_map(|atom| match side {
                SelectionSide::Original => atom.original,
                SelectionSide::Modified => atom.modified,
            })
            .collect();
        indices.sort_unstable();
        indices.dedup();
        let mut ranges: Vec<Range<usize>> = Vec::new();
        for index in indices {
            if let Some(last) = ranges.last_mut().filter(|last| last.end == index) {
                last.end += 1;
            } else {
                ranges.push(index..index + 1);
            }
        }

        let actual = select_atoms(
            &LineSelection::ranges(side, ranges.clone()),
            &projection.edits,
            &projection.atoms,
            projection.original_ids.len(),
            projection.modified_ids.len(),
        )?;
        let actual: HashSet<_> = actual.into_iter().collect();
        if actual.len() == desired.len() && desired.iter().all(|atom| actual.contains(atom)) {
            return ranges
                .into_iter()
                .map(|range| {
                    Ok(crate::LineRange {
                        side,
                        begin: u32::try_from(range.start + 1)?,
                        end: u32::try_from(range.end)?,
                    })
                })
                .collect();
        }
    }

    anyhow::bail!(
        "The batch cannot represent the exact selected lines. Reopen the diff and select the changes again."
    )
}

pub(crate) fn combined_selection(
    ranges: &[crate::LineRange],
    projection: &SelectionProjection,
) -> Result<LineSelection> {
    let mut anchors = Vec::new();
    for range in ranges {
        ensure!(
            range.begin > 0 && range.begin <= range.end,
            "Invalid line selection"
        );
    }

    for side in [SelectionSide::Original, SelectionSide::Modified] {
        let selection = LineSelection::ranges(
            side,
            ranges
                .iter()
                .filter(|range| range.side == side)
                .map(|range| Ok(usize::try_from(range.begin - 1)?..usize::try_from(range.end)?))
                .collect::<Result<Vec<_>>>()?,
        );
        anchors.extend(
            select_atoms(
                &selection,
                &projection.edits,
                &projection.atoms,
                projection.original_ids.len(),
                projection.modified_ids.len(),
            )?
            .into_iter()
            .map(|atom| LineAnchor {
                original: atom.original,
                modified: atom.modified,
            }),
        );
    }

    Ok(LineSelection::anchored(
        ranges
            .first()
            .map_or(SelectionSide::Modified, |range| range.side),
        anchors,
    ))
}

pub fn projection_between(
    original: &str,
    modified: &str,
    next_original: &str,
    next_modified: &str,
) -> Result<Option<SelectionProjection>> {
    let Some(edits) = local_line_edits(original, modified) else {
        return Ok(None);
    };
    let Some(head_edits) = local_line_edits(modified, next_modified) else {
        return Ok(None);
    };
    let Some(base_edits) = local_line_edits(original, next_original) else {
        return Ok(None);
    };
    let Some(remaining_edits) = local_line_edits(next_original, next_modified) else {
        return Ok(None);
    };
    let first = FileStep {
        before: Some(original.into()),
        after: Some(modified.into()),
        edits: edits.clone(),
        edits_to_head: Vec::new(),
    };
    let whole = LineSelection::whole_file();
    let previous = plan_selection(
        Some(original),
        Some(modified),
        &edits,
        &whole,
        std::slice::from_ref(&first),
        SelectionPlanOptions::default(),
    )?
    .projection;
    let next = plan_selection(
        Some(original),
        Some(next_modified),
        &[],
        &whole,
        &[
            first,
            FileStep {
                before: Some(modified.into()),
                after: Some(next_modified.into()),
                edits: head_edits,
                edits_to_head: Vec::new(),
            },
        ],
        SelectionPlanOptions::default(),
    )?
    .projection;
    let next_base = plan_selection(
        Some(original),
        Some(next_original),
        &base_edits,
        &whole,
        &[FileStep {
            before: Some(original.into()),
            after: Some(next_original.into()),
            edits: base_edits.clone(),
            edits_to_head: Vec::new(),
        }],
        SelectionPlanOptions::default(),
    )?
    .projection;
    let mut fresh = next
        .modified_ids
        .iter()
        .copied()
        .max()
        .map_or(0, |id| id + 1);
    let destination_ids = next_base
        .modified_ids
        .into_iter()
        .map(|id| {
            if id < previous.original_ids.len() {
                id
            } else {
                let result = fresh;
                fresh += 1;
                result
            }
        })
        .collect();
    Ok(Some(SelectionProjection {
        destination_ids,
        modified_destination_ids: Some(next.modified_ids),
        remaining_edits,
        ..previous
    }))
}

pub fn transfer_text(
    working: &str,
    original: &str,
    modified: &str,
    selection: &LineSelection,
    remove: bool,
) -> Result<Option<String>> {
    if selection.whole_file && remove {
        return Ok(Some(original.into()));
    }

    let Some(edits) = local_line_edits(original, modified) else {
        return Ok(None);
    };
    let scoped = plan_selection(
        Some(original),
        Some(modified),
        &edits,
        &LineSelection::whole_file(),
        &[FileStep {
            before: Some(original.into()),
            after: Some(modified.into()),
            edits: edits.clone(),
            edits_to_head: Vec::new(),
        }],
        SelectionPlanOptions::default(),
    )?;
    let selected = select_atoms(
        selection,
        &edits,
        &scoped.projection.atoms,
        text_lines(original).len(),
        text_lines(modified).len(),
    )?;
    if selected.is_empty() {
        return Ok(Some(working.into()));
    }

    let (source, target) = if remove {
        (modified, original)
    } else {
        (original, modified)
    };
    let Some(first) = local_line_edits(source, working) else {
        return Ok(None);
    };
    let Some(second) = local_line_edits(working, target) else {
        return Ok(None);
    };
    let Some(full) = local_line_edits(source, target) else {
        return Ok(None);
    };
    let chosen = LineSelection::anchored(
        SelectionSide::Modified,
        selected
            .iter()
            .map(|atom| {
                if remove {
                    LineAnchor {
                        original: atom.modified,
                        modified: atom.original,
                    }
                } else {
                    LineAnchor {
                        original: atom.original,
                        modified: atom.modified,
                    }
                }
            })
            .collect(),
    );
    let plan = plan_selection(
        Some(source),
        Some(target),
        &full,
        &chosen,
        &[
            FileStep {
                before: Some(source.into()),
                after: Some(working.into()),
                edits: first,
                edits_to_head: second.clone(),
            },
            FileStep {
                before: Some(working.into()),
                after: Some(target.into()),
                edits: second,
                edits_to_head: Vec::new(),
            },
        ],
        SelectionPlanOptions::default(),
    )?;
    Ok(Some(
        plan.steps
            .first()
            .context("Missing working-copy projection")?
            .clone()
            .unwrap_or_default(),
    ))
}

#[cfg(test)]
#[path = "selection_tests.rs"]
mod tests;

pub(crate) fn native_line_edits(
    diff: &str,
    before: Option<&str>,
    after: Option<&str>,
) -> Result<Vec<LineEdit>> {
    let header = regex::Regex::new(r"^@@ -([0-9]+)(?:,([0-9]+))? \+([0-9]+)(?:,([0-9]+))? @@")?;
    let mut edits = Vec::new();
    for line in diff.lines().filter(|line| line.starts_with("@@")) {
        let captures = header
            .captures(line)
            .context("Invalid native diff header")?;
        let range = |start_index, count_index| -> Result<Range<usize>> {
            let start = captures
                .get(start_index)
                .context("Missing native diff position")?
                .as_str()
                .parse::<usize>()?;
            let count = captures
                .get(count_index)
                .map(|value| value.as_str().parse::<usize>())
                .transpose()?
                .unwrap_or(1);
            let start = if count == 0 {
                start
            } else {
                start
                    .checked_sub(1)
                    .context("Invalid native diff position")?
            };
            let end = start
                .checked_add(count)
                .context("Native diff range overflow")?;
            Ok(start..end)
        };
        edits.push(LineEdit {
            original: range(1, 2)?,
            modified: range(3, 4)?,
        });
    }
    ensure!(
        apply_atoms(
            &text_lines(before.unwrap_or_default()),
            &text_lines(after.unwrap_or_default()),
            &positional_atoms(&edits),
        )?
        .concat()
            == after.unwrap_or_default(),
        "The native diff does not reproduce the selected file"
    );
    Ok(edits)
}

pub(crate) fn project_history(
    snapshots: &[Option<String>],
    adjacent: &[Vec<LineEdit>],
    aggregate: &[LineEdit],
    to_target: &[Vec<LineEdit>],
    base_index: usize,
    ranges: Option<&[crate::LineRange]>,
    forward: bool,
) -> Result<Vec<Option<String>>> {
    let head_index = snapshots
        .len()
        .checked_sub(1)
        .context("Empty source history")?;
    ensure!(
        adjacent.len() == head_index && to_target.len() == snapshots.len(),
        "Incomplete source diffs"
    );
    let text = |index: usize| -> Result<Vec<&str>> {
        Ok(text_lines(
            snapshots
                .get(index)
                .context("Invalid history index")?
                .as_deref()
                .unwrap_or_default(),
        ))
    };
    let original = text(base_index)?;
    let modified = text(head_index)?;
    let mut next_id = text(0)?.len();
    let mut identities = vec![(0..next_id).collect::<Vec<_>>()];
    for (index, edits) in adjacent.iter().enumerate() {
        let before = identities.get(index).context("Missing source identities")?;
        let mut after = Vec::new();
        let mut offset = 0;
        for edit in edits {
            after.extend_from_slice(
                before
                    .get(offset..edit.original.start)
                    .context("Invalid source diff")?,
            );
            for added in 0..edit.modified.len() {
                if added < edit.original.len() {
                    after.push(
                        *before
                            .get(edit.original.start + added)
                            .context("Invalid replacement")?,
                    );
                } else {
                    after.push(next_id);
                    next_id += 1;
                }
            }
            offset = edit.original.end;
        }
        after.extend_from_slice(before.get(offset..).context("Invalid source diff end")?);
        ensure!(
            after.len() == text(index + 1)?.len(),
            "Source identity count changed"
        );
        identities.push(after);
    }
    let base_ids = identities
        .get(base_index)
        .context("Invalid source baseline")?;
    let head_ids = identities.get(head_index).context("Missing source head")?;
    let candidates = lineage_atoms(aggregate, base_ids, head_ids)?;
    let mut selected = HashSet::new();
    if let Some(ranges) = ranges {
        ensure!(
            !ranges.is_empty()
                && ranges
                    .iter()
                    .all(|range| range.begin > 0 && range.begin <= range.end),
            "Invalid line selection"
        );
        for side in [SelectionSide::Original, SelectionSide::Modified] {
            let selected_ranges = ranges
                .iter()
                .filter(|range| range.side == side)
                .map(|range| (range.begin as usize - 1)..range.end as usize)
                .collect::<Vec<_>>();
            if selected_ranges.is_empty() {
                continue;
            }
            for atom in select_atoms(
                &LineSelection::ranges(side, selected_ranges),
                aggregate,
                &candidates,
                original.len(),
                modified.len(),
            )? {
                selected.insert(atom);
            }
        }
    } else {
        selected.extend(candidates);
    }
    let mut selected_ids = HashSet::new();
    for atom in selected {
        if let Some(index) = atom.original {
            selected_ids.insert(*base_ids.get(index).context("Invalid original line")?);
        }
        if let Some(index) = atom.modified {
            selected_ids.insert(*head_ids.get(index).context("Invalid modified line")?);
        }
    }
    let target_index = if forward { base_index } else { head_index };
    let target = text(target_index)?;
    let target_ids = identities
        .get(target_index)
        .context("Missing target identities")?;
    let target_snapshot = snapshots
        .get(target_index)
        .context("Missing target snapshot")?;
    let mut result = Vec::new();
    for (index, ((snapshot, edits), source_ids)) in
        snapshots.iter().zip(to_target).zip(&identities).enumerate()
    {
        if ranges.is_none() && (forward || index >= base_index) {
            result.push(target_snapshot.clone());
            continue;
        }
        let changes = lineage_atoms(edits, source_ids, target_ids)?
            .into_iter()
            .filter(|atom| {
                atom.original
                    .and_then(|line| source_ids.get(line))
                    .is_some_and(|id| selected_ids.contains(id))
                    || atom
                        .modified
                        .and_then(|line| target_ids.get(line))
                        .is_some_and(|id| selected_ids.contains(id))
            })
            .collect::<Vec<_>>();
        let projected = apply_atoms(&text(index)?, &target, &changes)?.concat();
        let exists = target_snapshot.is_some() && (snapshot.is_some() || ranges.is_none());
        result.push(if projected.is_empty() && !exists {
            None
        } else {
            Some(projected)
        });
    }
    Ok(result)
}
