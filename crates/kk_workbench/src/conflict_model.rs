use crate::{ConflictRole, FileConflict, local_line_edits, parse_conflict_hunks};
use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::{collections::HashMap, ops::Range};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PresentedSection {
    pub role: ConflictRole,
    pub index: usize,
    pub label: String,
    pub range: Range<usize>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PresentedConflict {
    pub range: Range<usize>,
    pub sections: Vec<PresentedSection>,
    pub edited: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct PresentationState {
    digest: String,
    conflicts: Vec<PresentedConflict>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConflictEdit {
    pub range: Range<usize>,
    pub text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictEditReason {
    Edit,
    Undo,
    Redo,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictChoice {
    Input(ConflictRole, usize),
    Edited,
}

#[derive(Clone, Debug)]
pub struct ConflictAcceptance {
    pub edits: Vec<ConflictEdit>,
    pub starts: Vec<usize>,
    before: String,
    raw_result: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ConflictDraft {
    pub conflict: FileConflict,
    pub text: String,
    pub saved_text: String,
    pub conflicts: Vec<PresentedConflict>,
    pub applied_digest: Option<String>,
    pub applied_deleted: bool,
    history: Vec<PresentationState>,
    cursor: usize,
    saved_conflicts: Vec<PresentedConflict>,
    byte_versions: HashMap<String, String>,
}

impl PresentedSection {
    pub fn is_empty_base(&self) -> bool {
        self.role == ConflictRole::Base && self.range.is_empty()
    }
}

impl ConflictDraft {
    pub fn open(conflict: FileConflict) -> Result<Self> {
        let raw = conflict.text()?;
        let hunks = parse_conflict_hunks(raw, conflict.marker_length)?;
        let mut text = String::new();
        let mut offset = 0;
        let mut conflicts = Vec::new();
        for hunk in hunks {
            text.push_str(&raw[offset..hunk.range.start]);
            let start = normalize_conflict_text(&text).len();
            let line_ending = if raw[hunk.range.clone()].contains("\r\n") {
                "\r\n"
            } else {
                "\n"
            };
            let mut sections = Vec::new();
            for section in hunk.sections {
                let start = normalize_conflict_text(&text).len();
                text.push_str(&section.text);
                let end = normalize_conflict_text(&text).len();
                if !section.text.ends_with('\n')
                    && !(section.role == ConflictRole::Base && section.text.is_empty())
                {
                    text.push_str(line_ending);
                }

                sections.push(PresentedSection {
                    role: section.role,
                    index: section.index,
                    label: section.label,
                    range: start..end,
                });
            }

            conflicts.push(PresentedConflict {
                range: start..normalize_conflict_text(&text).len(),
                sections,
                edited: false,
            });
            offset = hunk.range.end;
        }

        text.push_str(&raw[offset..]);
        let mut draft = Self {
            conflict,
            saved_text: text.clone(),
            text,
            saved_conflicts: conflicts.clone(),
            conflicts,
            applied_digest: None,
            applied_deleted: false,
            history: Vec::new(),
            cursor: 0,
            byte_versions: HashMap::new(),
        };
        draft.record();

        Ok(draft)
    }

    pub fn editor_text(&self) -> String {
        normalize_conflict_text(&self.text)
    }

    pub fn is_dirty(&self) -> bool {
        self.text != self.saved_text || self.conflicts != self.saved_conflicts
    }

    pub fn mark_saved(&mut self) {
        self.saved_text.clone_from(&self.text);
        self.saved_conflicts.clone_from(&self.conflicts);
    }

    pub fn acknowledge_save(&mut self, saved: &Self) {
        self.saved_text.clone_from(&saved.saved_text);
        self.saved_conflicts.clone_from(&saved.saved_conflicts);
    }

    pub fn revert_to_saved(&mut self) {
        self.text.clone_from(&self.saved_text);
        self.conflicts.clone_from(&self.saved_conflicts);
        self.record();
    }

    pub fn mark_applied(&mut self, submitted: &str, deleted: bool) {
        self.applied_digest = Some(digest(submitted));
        self.applied_deleted = deleted;
    }

    pub fn has_newer_edits(&self) -> bool {
        self.applied_digest
            .as_ref()
            .is_some_and(|applied| *applied != digest(&self.text))
    }

    pub fn update(
        &mut self,
        next: &str,
        edits: Option<&[ConflictEdit]>,
        reason: ConflictEditReason,
    ) -> Result<()> {
        let next = normalize_conflict_text(next);
        let previous = self.editor_text();
        if previous == next {
            return Ok(());
        }

        let next_digest = digest(&next);
        let restored = match reason {
            ConflictEditReason::Undo => (0..self.cursor).rev().find(|index| {
                self.history
                    .get(*index)
                    .is_some_and(|state| state.digest == next_digest)
            }),
            ConflictEditReason::Redo => (self.cursor + 1..self.history.len()).find(|index| {
                self.history
                    .get(*index)
                    .is_some_and(|state| state.digest == next_digest)
            }),
            ConflictEditReason::Edit => None,
        };
        let raw = match self.byte_versions.get(&next_digest) {
            Some(raw) => raw.clone(),
            None => edit_raw_result(&self.text, &next)?,
        };
        if let Some(index) = restored {
            self.conflicts = self.history[index].conflicts.clone();
            self.cursor = index;
            self.text = raw;
            return Ok(());
        }

        let fallback;
        let edits = if let Some(edits) = edits.filter(|edits| !edits.is_empty()) {
            edits
        } else {
            fallback = vec![replacement_edit(&previous, &next)];
            &fallback
        };
        let mut ordered = edits.to_vec();
        ordered.sort_by_key(|edit| std::cmp::Reverse(edit.range.start));
        let mut expected = previous.clone();
        let mut boundary = previous.len();
        for edit in &ordered {
            ensure!(
                edit.range.start <= edit.range.end
                    && edit.range.end <= boundary
                    && expected.get(edit.range.clone()).is_some(),
                "Invalid conflict edit range"
            );
            expected.replace_range(edit.range.clone(), &normalize_conflict_text(&edit.text));
            boundary = edit.range.start;
        }

        ensure!(
            expected == next,
            "Conflict edit positions do not match the editor text"
        );
        for edit in &ordered {
            self.move_boundaries(edit);
        }

        let mut merged: Vec<PresentedConflict> = Vec::new();
        for conflict in self.conflicts.drain(..) {
            if let Some(previous) = merged.last_mut()
                && (previous.range.end > conflict.range.start
                    || (previous.edited && previous.range == conflict.range))
            {
                previous.range.end = previous.range.end.max(conflict.range.end);
                previous.edited = true;
                previous.sections.clear();
            } else {
                merged.push(conflict);
            }
        }

        self.conflicts = merged;
        self.text = raw;
        self.record();

        Ok(())
    }

    pub fn prepare_accept(
        &self,
        starts: &[usize],
        choice: ConflictChoice,
    ) -> Result<ConflictAcceptance> {
        ensure!(!starts.is_empty(), "Select a conflict block");
        ensure!(
            self.applied_digest.is_none(),
            "This resolution has already been applied"
        );
        let before = self.editor_text();
        let mut edits = Vec::new();
        let mut replacements = Vec::new();
        let mut starts = starts.to_vec();
        starts.sort_unstable();
        starts.dedup();
        for start in &starts {
            let conflict = self
                .conflicts
                .iter()
                .find(|conflict| conflict.range.start == *start)
                .context("The selected conflict block is no longer available")?;
            let range = match choice {
                ConflictChoice::Edited => {
                    ensure!(conflict.edited, "Use Edited Block requires an explicitly edited block");
                    &conflict.range
                }
                ConflictChoice::Input(role, index) => &conflict.sections.iter().find(|section| section.role == role && section.index == index)
                    .context("A conflict block was edited across inputs. Confirm that edited block before accepting a version for all remaining blocks.")?.range,
            };
            let physical = physical_range(&self.text, range)?;
            let replacement = self
                .text
                .get(physical)
                .context("Invalid conflict input position")?
                .to_owned();
            edits.push(ConflictEdit {
                range: conflict.range.clone(),
                text: normalize_conflict_text(&replacement),
            });
            replacements.push((physical_range(&self.text, &conflict.range)?, replacement));
        }

        let mut raw_result = self.text.clone();
        for (range, replacement) in replacements.into_iter().rev() {
            raw_result.replace_range(range, &replacement);
        }

        Ok(ConflictAcceptance {
            edits,
            starts,
            before,
            raw_result,
        })
    }

    pub fn accept(&mut self, acceptance: ConflictAcceptance) -> Result<()> {
        ensure!(
            self.editor_text() == acceptance.before,
            "This conflict changed. Select the action again."
        );
        let remaining: Vec<_> = self
            .conflicts
            .iter()
            .filter(|conflict| !acceptance.starts.contains(&conflict.range.start))
            .cloned()
            .collect();
        let next = normalize_conflict_text(&acceptance.raw_result);
        self.conflicts = remaining;
        let unchanged = acceptance.before == next;
        self.update(&next, Some(&acceptance.edits), ConflictEditReason::Edit)?;
        self.text = acceptance.raw_result;
        self.byte_versions.insert(digest(&next), self.text.clone());
        if unchanged {
            self.record();
        }

        Ok(())
    }

    pub fn line_numbers(&self) -> Vec<Option<u32>> {
        let text = self.editor_text();
        let lines: Vec<_> = text.split('\n').collect();
        if self.conflicts.is_empty() {
            return (1..=lines.len()).map(|row| Some(row as u32)).collect();
        }

        let mut starts = vec![0];
        starts.extend(text.match_indices('\n').map(|(offset, _)| offset + 1));
        let row_at = |offset| {
            starts
                .partition_point(|start| *start <= offset)
                .saturating_sub(1)
        };
        let mut included = vec![true; lines.len()];
        for conflict in &self.conflicts {
            if conflict.edited || conflict.range.is_empty() {
                continue;
            }

            for row in row_at(conflict.range.start)..=row_at(conflict.range.end - 1) {
                if let Some(included) = included.get_mut(row) {
                    *included = false;
                }
            }

            if let Some(base) = conflict
                .sections
                .iter()
                .find(|section| section.role == ConflictRole::Base && section.index == 0)
                && !base.range.is_empty()
                && starts.get(row_at(base.range.start)) == Some(&base.range.start)
            {
                for row in row_at(base.range.start)..=row_at(base.range.end - 1) {
                    if let Some(included) = included.get_mut(row) {
                        *included = true;
                    }
                }
            }
        }

        let rows: Vec<_> = included
            .iter()
            .enumerate()
            .filter_map(|(row, included)| {
                (*included && !(row == lines.len() - 1 && text.ends_with('\n')) && !text.is_empty())
                    .then_some(row)
            })
            .collect();
        let mut labels = vec![None; lines.len()];
        let Some(base) = self
            .conflict
            .inputs
            .iter()
            .find(|input| input.role == ConflictRole::Base && input.index == 0)
            .and_then(|input| std::str::from_utf8(&input.bytes).ok())
        else {
            return labels;
        };
        let original = normalize_conflict_text(base);
        if original.is_empty() || rows.is_empty() {
            return labels;
        }

        let mut projected = rows
            .iter()
            .filter_map(|row| lines.get(*row))
            .copied()
            .collect::<Vec<_>>()
            .join("\n");
        projected = projected.trim_start_matches('\u{feff}').to_owned();
        projected.push('\n');
        let original = format!("{}\n", original.strip_suffix('\n').unwrap_or(&original));
        let Some(edits) = local_line_edits(&original, &projected) else {
            return labels;
        };
        let (mut source, mut target) = (0, 0);
        for edit in edits.iter().chain(std::iter::once(&crate::LineEdit {
            original: original.lines().count()..original.lines().count(),
            modified: rows.len()..rows.len(),
        })) {
            while source < edit.original.start && target < edit.modified.start {
                if let Some(row) = rows.get(target)
                    && let Some(label) = labels.get_mut(*row)
                {
                    *label = Some(source as u32 + 1);
                }

                source += 1;
                target += 1;
            }

            source = edit.original.end;
            target = edit.modified.end;
        }

        labels
    }

    fn record(&mut self) {
        let hash = digest(&self.editor_text());
        self.history.truncate(self.cursor + 1);
        self.history.push(PresentationState {
            digest: hash.clone(),
            conflicts: self.conflicts.clone(),
        });
        self.cursor = self.history.len() - 1;
        self.byte_versions.insert(hash, self.text.clone());
    }

    fn move_boundaries(&mut self, edit: &ConflictEdit) {
        let length = normalize_conflict_text(&edit.text).len();
        let delta = length as isize - edit.range.len() as isize;
        let shift = |offset: usize| offset.saturating_add_signed(delta);
        let position = |offset: usize, right: bool| {
            if offset < edit.range.start {
                offset
            } else if offset > edit.range.end {
                shift(offset)
            } else {
                edit.range.start + if right { length } else { 0 }
            }
        };
        for conflict in &mut self.conflicts {
            if edit.range.end <= conflict.range.start && edit.range.start < conflict.range.start {
                conflict.range = shift(conflict.range.start)..shift(conflict.range.end);
                for section in &mut conflict.sections {
                    section.range = shift(section.range.start)..shift(section.range.end);
                }

                continue;
            }

            if edit.range.start >= conflict.range.end {
                continue;
            }

            let selected = conflict
                .sections
                .iter()
                .position(|section| {
                    !section.is_empty_base()
                        && edit.range.start == section.range.start
                        && edit.range.end <= section.range.end
                })
                .or_else(|| {
                    conflict.sections.iter().position(|section| {
                        !section.is_empty_base()
                            && edit.range.start >= section.range.start
                            && edit.range.end <= section.range.end
                    })
                });
            if conflict.edited || selected.is_none() {
                conflict.edited = true;
                conflict.sections.clear();
                conflict.range.start = conflict.range.start.min(edit.range.start);
                conflict.range.end =
                    position(conflict.range.end, true).max(edit.range.start + length);
                continue;
            }

            for (index, section) in conflict.sections.iter_mut().enumerate() {
                if section.is_empty_base() {
                    let offset = position(section.range.start, false);
                    section.range = offset..offset;
                } else if Some(index) == selected {
                    section.range.end = shift(section.range.end);
                } else if section.range.start >= edit.range.end {
                    section.range = shift(section.range.start)..shift(section.range.end);
                }
            }

            conflict.range.end = shift(conflict.range.end);
        }
    }
}

pub fn normalize_conflict_text(text: &str) -> String {
    text.strip_prefix('\u{feff}')
        .unwrap_or(text)
        .replace("\r\n", "\n")
}

fn digest(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

fn replacement_edit(previous: &str, next: &str) -> ConflictEdit {
    let mut start = previous
        .bytes()
        .zip(next.bytes())
        .take_while(|(before, after)| before == after)
        .count();
    while !previous.is_char_boundary(start) || !next.is_char_boundary(start) {
        start -= 1;
    }

    let mut suffix = previous[start..]
        .bytes()
        .rev()
        .zip(next[start..].bytes().rev())
        .take_while(|(before, after)| before == after)
        .count();
    while !previous.is_char_boundary(previous.len() - suffix)
        || !next.is_char_boundary(next.len() - suffix)
    {
        suffix -= 1;
    }

    ConflictEdit {
        range: start..previous.len() - suffix,
        text: next[start..next.len() - suffix].into(),
    }
}

fn physical_range(text: &str, range: &Range<usize>) -> Result<Range<usize>> {
    let offset = |logical: usize| -> Result<usize> {
        let mut physical = if text.starts_with('\u{feff}') {
            '\u{feff}'.len_utf8()
        } else {
            0
        };
        let mut count = 0;
        while count < logical {
            let tail = text.get(physical..).context("Invalid conflict position")?;
            let character = tail
                .chars()
                .next()
                .context("Conflict position exceeds text")?;
            physical += if tail.starts_with("\r\n") {
                2
            } else {
                character.len_utf8()
            };
            count += if tail.starts_with("\r\n") {
                1
            } else {
                character.len_utf8()
            };
        }

        ensure!(
            count == logical,
            "Conflict position splits a UTF-8 character"
        );
        Ok(physical)
    };
    Ok(offset(range.start)?..offset(range.end)?)
}

fn edit_raw_result(previous: &str, next: &str) -> Result<String> {
    let edit = replacement_edit(&normalize_conflict_text(previous), next);
    let range = physical_range(previous, &edit.range)?;
    let mut raw = previous.to_owned();
    let replacement = if previous.contains("\r\n") {
        edit.text.replace('\n', "\r\n")
    } else {
        edit.text
    };
    raw.replace_range(range, &replacement);

    Ok(raw)
}
