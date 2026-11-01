use super::*;
use crate::LineRange;
use crate::selection::{native_line_edits, project_history};
use crate::{ClassificationPreview, TransferDirection};

fn edits(before: &str, after: &str) -> Vec<LineEdit> {
    local_line_edits(before, after).expect("small fixture alignment")
}

fn plan(before: &str, after: &str, selection: &LineSelection) -> SelectionPlan {
    let changes = edits(before, after);
    plan_selection(
        Some(before),
        Some(after),
        &changes,
        selection,
        &[FileStep {
            before: Some(before.into()),
            after: Some(after.into()),
            edits: changes.clone(),
            edits_to_head: Vec::new(),
        }],
        SelectionPlanOptions::default(),
    )
    .expect("valid selection")
}

#[test]
fn alignment_roundtrips_repeated_lines_and_line_endings() {
    let mut texts = vec![String::new()];
    for length in 1..=5 {
        for bits in 0..1 << length {
            texts.push(
                (0..length)
                    .map(|index| {
                        if bits & (1 << index) == 0 {
                            "a\n"
                        } else {
                            "b\n"
                        }
                    })
                    .collect(),
            );
        }
    }

    texts.extend(["a", "a\nb", "a\r\nb\r\n", "\n", "\r\n"].map(str::to_owned));
    for before in &texts {
        for after in &texts {
            let changes = edits(before, after);
            assert_eq!(
                apply_atoms(
                    &text_lines(before),
                    &text_lines(after),
                    &positional_atoms(&changes)
                )
                .unwrap()
                .concat(),
                *after,
                "{before:?} -> {after:?}"
            );
        }
    }

    assert!(local_line_edits(&"a\n".repeat(600), &"b\n".repeat(600)).is_none());
}

#[test]
fn optimistic_replacement_matrix_preserves_old_coordinates_and_independent_rollback() {
    for original_count in [1, 2, 3, 5] {
        for modified_count in [1, 2, 3, 5] {
            for side in [SelectionSide::Original, SelectionSide::Modified] {
                for reverse in [false, true] {
                    for acknowledge in [false, true] {
                        for repeated in [false, true] {
                            let before: String = (0..original_count)
                                .map(|index| {
                                    if repeated {
                                        "old\n".into()
                                    } else {
                                        format!("old {index}\n")
                                    }
                                })
                                .collect();
                            let after: String = (0..modified_count)
                                .map(|index| {
                                    if repeated {
                                        "NEW\n".into()
                                    } else {
                                        format!("NEW {index}\n")
                                    }
                                })
                                .collect();
                            let mut model = ClassificationPreview::new(&before, &after).unwrap();
                            let length = if side == SelectionSide::Original {
                                original_count
                            } else {
                                modified_count
                            };
                            let mut order: Vec<_> = (0..length).collect();
                            if reverse {
                                order.reverse();
                            }

                            for (id, line) in order.iter().enumerate() {
                                let selection = model
                                    .capture(
                                        &before,
                                        &after,
                                        &LineSelection::ranges(side, vec![*line..*line + 1]),
                                    )
                                    .unwrap();
                                model
                                    .add(id as u64, selection, TransferDirection::Backward)
                                    .unwrap();
                                if acknowledge {
                                    let mut plan = plan(
                                        model.confirmed(),
                                        &after,
                                        &model.selection(id as u64).unwrap().unwrap(),
                                    );
                                    let selected = plan.selected_base.as_deref().unwrap();
                                    plan.projection.remaining_edits = edits(selected, &after);
                                    model
                                        .accept(selected, &after, &plan.projection, &[id as u64])
                                        .unwrap();
                                }
                            }

                            assert_eq!(
                                model.text(),
                                after,
                                "counts {original_count}/{modified_count}, {side:?}, reverse {reverse}, acknowledge {acknowledge}, repeated {repeated}"
                            );
                            if !acknowledge && length > 1 {
                                model.remove(0).unwrap();
                                let mut isolated =
                                    ClassificationPreview::new(&before, &after).unwrap();
                                for line in order.iter().skip(1) {
                                    let selection = isolated
                                        .capture(
                                            &before,
                                            &after,
                                            &LineSelection::ranges(side, vec![*line..*line + 1]),
                                        )
                                        .unwrap();
                                    isolated
                                        .add(*line as u64, selection, TransferDirection::Backward)
                                        .unwrap();
                                }

                                assert_eq!(model.text(), isolated.text());
                                assert_ne!(model.text(), after);
                            }
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn forward_preview_and_external_reconciliation() {
    let before = "a\nkeep\nb\n";
    let after = "A\nkeep\nB\n";
    let mut model = ClassificationPreview::new(before, after).unwrap();
    let first = model
        .capture(
            before,
            after,
            &LineSelection::ranges(SelectionSide::Modified, vec![0..1]),
        )
        .unwrap();
    let second = model
        .capture(
            before,
            after,
            &LineSelection::ranges(SelectionSide::Modified, vec![2..3]),
        )
        .unwrap();
    model
        .add(1, first.clone(), TransferDirection::Forward)
        .unwrap();
    model.add(2, second, TransferDirection::Backward).unwrap();
    assert_eq!(model.text(), "a\nkeep\nB\n");
    assert_eq!(model.modified_text(), "a\nkeep\nB\n");
    model.add(3, first, TransferDirection::Backward).unwrap();
    assert_eq!(model.text(), "a\nkeep\nB\n");
    model.remove(1).unwrap();
    assert_eq!(model.text(), after);
    assert_eq!(model.modified_text(), after);
    assert_eq!(model.reconcile_external("new\n", "NEW\n"), vec![2, 3]);
    assert!(!model.is_pending());
    assert!(!model.knows(before, after));
    assert_eq!(model.text(), "new\n");
}

#[test]
fn native_ranges_preserve_completed_short_side_without_extra_atoms() {
    let projection = plan("a\nb\n", "A\nB\nC\n", &LineSelection::whole_file()).projection;
    let selection = combined_selection(
        &[
            crate::LineRange {
                side: SelectionSide::Original,
                begin: 1,
                end: 1,
            },
            crate::LineRange {
                side: SelectionSide::Original,
                begin: 2,
                end: 2,
            },
        ],
        &projection,
    )
    .unwrap();
    assert_eq!(
        native_selection_ranges(&selection, &projection).unwrap(),
        vec![crate::LineRange {
            side: SelectionSide::Original,
            begin: 1,
            end: 2
        }]
    );
    let partial = LineSelection::anchored(
        SelectionSide::Original,
        vec![
            LineAnchor {
                original: Some(0),
                modified: Some(0),
            },
            LineAnchor {
                original: Some(1),
                modified: Some(1),
            },
        ],
    );
    assert_eq!(
        native_selection_ranges(&partial, &projection).unwrap(),
        vec![crate::LineRange {
            side: SelectionSide::Modified,
            begin: 1,
            end: 2
        }]
    );
}

#[test]
fn selection_projects_through_intermediate_versions_and_rebases_remaining_lines() {
    let before = "a\nkeep\nb\n";
    let intermediate = "a1\nkeep\nb1\n";
    let after = "A\nkeep\nB\n";
    let selection = LineSelection::ranges(SelectionSide::Modified, vec![0..1]);
    let mut plan = plan_selection(
        Some(before),
        Some(after),
        &edits(before, after),
        &selection,
        &[
            FileStep {
                before: Some(before.into()),
                after: Some(intermediate.into()),
                edits: edits(before, intermediate),
                edits_to_head: edits(intermediate, after),
            },
            FileStep {
                before: Some(intermediate.into()),
                after: Some(after.into()),
                edits: edits(intermediate, after),
                edits_to_head: Vec::new(),
            },
        ],
        SelectionPlanOptions::default(),
    )
    .unwrap();
    assert_eq!(plan.selected_base.as_deref(), Some("A\nkeep\nb\n"));
    assert_eq!(
        plan.steps,
        vec![Some("A\nkeep\nb1\n".into()), Some(after.into())]
    );
    plan.projection.remaining_edits = edits(plan.selected_base.as_deref().unwrap(), after);
    assert!(
        rebase_selection(&selection, &plan.projection)
            .unwrap()
            .anchors
            .unwrap()
            .is_empty()
    );
    let remaining = rebase_selection(
        &LineSelection::ranges(SelectionSide::Modified, vec![2..3]),
        &plan.projection,
    )
    .unwrap();
    assert_eq!(
        remaining.anchors.unwrap(),
        vec![LineAnchor {
            original: Some(2),
            modified: Some(2)
        }]
    );
    assert_eq!(
        transfer_text("a\nkeep\nother\n", before, after, &selection, false)
            .unwrap()
            .as_deref(),
        Some("A\nkeep\nother\n")
    );
}

#[test]
fn whole_file_preserves_absence_and_eof_cursor_selects_final_hunk() {
    let changes = edits("present\n", "");
    let plan = plan_selection(
        Some("present\n"),
        None,
        &changes,
        &LineSelection::whole_file(),
        &[FileStep {
            before: Some("present\n".into()),
            after: None,
            edits: changes.clone(),
            edits_to_head: Vec::new(),
        }],
        SelectionPlanOptions::default(),
    )
    .unwrap();
    assert_eq!(plan.selected_base, None);
    assert_eq!(plan.steps, vec![None]);
    let mut selection = LineSelection::ranges(SelectionSide::Modified, Vec::new());
    selection.cursor = Some(2);
    assert_eq!(
        plan_selection(
            Some("a\nb\n"),
            Some("a\nB\n"),
            &edits("a\nb\n", "a\nB\n"),
            &selection,
            &[FileStep {
                before: Some("a\nb\n".into()),
                after: Some("a\nB\n".into()),
                edits: edits("a\nb\n", "a\nB\n"),
                edits_to_head: Vec::new(),
            }],
            SelectionPlanOptions::default()
        )
        .unwrap()
        .selected_base
        .as_deref(),
        Some("a\nB\n")
    );
}

#[test]
fn native_diff_coordinates_preserve_empty_sides_and_reject_incomplete_edits() -> Result<()> {
    assert_eq!(
        native_line_edits("@@ -0,0 +1,2 @@\n+a\n+b\n", None, Some("a\nb\n"))?,
        vec![LineEdit {
            original: 0..0,
            modified: 0..2
        }]
    );
    assert_eq!(
        native_line_edits("@@ -1,2 +0,0 @@\n-a\n-b\n", Some("a\nb\n"), None)?,
        vec![LineEdit {
            original: 0..2,
            modified: 0..0
        }]
    );
    assert_eq!(
        native_line_edits(
            "@@ -2 +2 @@\n-old\n+new\n",
            Some("keep\nold"),
            Some("keep\nnew")
        )?,
        vec![LineEdit {
            original: 1..2,
            modified: 1..2
        }]
    );
    assert!(native_line_edits("", Some("old\n"), Some("new\n")).is_err());
    assert!(native_line_edits("@@ -0 +1 @@", Some("old\n"), Some("new\n")).is_err());
    assert!(native_line_edits("@@ malformed", Some("old\n"), Some("new\n")).is_err());
    Ok(())
}

#[test]
fn history_projection_preserves_unselected_intermediate_edits_in_both_directions() -> Result<()> {
    let texts = [
        "first\nkeep\nlast\n",
        "FIRST\nkeep\nlast\n",
        "FIRST AGAIN\nkeep\nLAST\n",
    ];
    let snapshots = texts
        .iter()
        .map(|text| Some((*text).to_owned()))
        .collect::<Vec<_>>();
    let adjacent = texts
        .windows(2)
        .map(|pair| edits(pair[0], pair[1]))
        .collect::<Vec<_>>();
    let aggregate = edits(texts[0], texts[2]);
    for forward in [false, true] {
        let target = if forward { texts[0] } else { texts[2] };
        let to_target = texts
            .iter()
            .map(|text| edits(text, target))
            .collect::<Vec<_>>();
        for side in [SelectionSide::Original, SelectionSide::Modified] {
            let ranges = [LineRange {
                side,
                begin: 3,
                end: 3,
            }];
            let projected = project_history(
                &snapshots,
                &adjacent,
                &aggregate,
                &to_target,
                0,
                Some(&ranges),
                forward,
            )?;
            let expected = if forward {
                [
                    "first\nkeep\nlast\n",
                    "FIRST\nkeep\nlast\n",
                    "FIRST AGAIN\nkeep\nlast\n",
                ]
            } else {
                [
                    "first\nkeep\nLAST\n",
                    "FIRST\nkeep\nLAST\n",
                    "FIRST AGAIN\nkeep\nLAST\n",
                ]
            };
            assert_eq!(projected, expected.map(|text| Some(text.to_owned())));
        }
    }
    Ok(())
}
