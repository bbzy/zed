use super::*;
use crate::ConflictRole::{Base, Side};

pub(super) fn conflict_fixture(
    base: &[u8],
    left: &[u8],
    right: &[u8],
    path: &str,
) -> Result<Fixture> {
    let fixture = Fixture::create()?;
    fs::write(fixture.root().join(path), base)?;
    fs::write(fixture.root().join("unselected.txt"), "unchanged\n")?;
    fixture.run(&["describe", "-m", "base"])?;
    let base = fixture.run(&["log", "-r", "@", "--no-graph", "-T", "commit_id"])?;
    fixture.run(&["new", "-m", "left"])?;
    fs::write(fixture.root().join(path), left)?;
    let left = fixture.run(&["log", "-r", "@", "--no-graph", "-T", "commit_id"])?;
    fixture.run(&["new", &base, "-m", "right"])?;
    fs::write(fixture.root().join(path), right)?;
    fixture.run(&["rebase", "-r", "@", "-d", &left])?;
    Ok(fixture)
}

#[test]
fn conflict_parsers_preserve_literal_markers_and_missing_newlines() -> Result<()> {
    let first = "a".repeat(40);
    let second = "b".repeat(40);
    let third = "c".repeat(40);
    let term = |id: &str| {
        format!("Some(File {{ id: FileId(\"{id}\"), executable: false, copy_id: CopyId(\"\") }})")
    };
    let mut inputs = parse_conflict_inputs(
        &format!(
            "file: Ok(Conflicted([{}, {}, {}, {}, None]))\n",
            term(&first),
            term(&second),
            term(&third),
            term(&first)
        ),
        "file",
    )?;
    assert_eq!(inputs.len(), 3);
    assert_eq!(inputs[0].id.as_deref(), Some(third.as_str()));
    assert_eq!(inputs[1].id.as_deref(), Some(second.as_str()));
    assert_eq!(inputs[2].id, None);
    inputs[0].bytes = b"<<<<<<< literal\n".to_vec();
    let length = conflict_marker_length(&inputs);
    assert_eq!(length, 11);
    let text = format!(
        "context\r\n{} conflict 1 of 1\r\n{} side\r\n<<<<<<< literal\r\n{} base\r\nbase\r\n{} side\r\nright\r\n{} conflict 1 of 1",
        "<".repeat(length),
        "+".repeat(length),
        "-".repeat(length),
        "+".repeat(length),
        ">".repeat(length)
    );
    let hunks = parse_conflict_hunks(&text, length)?;
    assert_eq!(hunks.len(), 1);
    assert_eq!(hunks[0].range, 9..text.len());
    assert_eq!(hunks[0].sections[0].text, "<<<<<<< literal");
    assert_eq!(hunks[0].sections[1].text, "base");
    assert_eq!(hunks[0].sections[2].text, "right");
    assert_eq!(
        hunks[0]
            .sections
            .iter()
            .map(|section| (section.role, section.index))
            .collect::<Vec<_>>(),
        [(Side, 0), (Base, 0), (Side, 1)]
    );
    assert!(parse_conflict_hunks("<<<<<<< conflict 1\nraw\n", 7).is_err());
    assert!(parse_conflict_hunks("+++++++ unexpected\n", 7).is_err());
    assert!(parse_conflict_hunks("short", length)?.is_empty());
    assert!(parse_conflict_hunks("", length)?.is_empty());
    assert!(parse_conflict_inputs("file: Ok(Resolved(None))", "file").is_err());
    Ok(())
}

#[test]
fn conflict_resolution_is_private_exact_and_one_undo() -> Result<()> {
    smol::block_on(async {
        let path = "quote\" $(do-not-run).txt";
        let fixture = conflict_fixture(
            b"header\r\nbase\r\nfooter\r\n",
            b"header\r\nleft\r\nfooter\r\n",
            b"header\r\nright\r\nfooter\r\n",
            path,
        )?;
        let repository = fixture.repository().await?;
        let snapshot = repository.snapshot(true).await?;
        let conflict = snapshot.file_conflict("@", path).await?;
        assert!(!conflict.binary);
        assert!(!conflict.immutable);
        assert_eq!(conflict.inputs.len(), 3);
        assert!(conflict.inputs.iter().all(|input| {
            input
                .source
                .as_ref()
                .is_some_and(|source| !source.details_incomplete && source.commit_id.len() >= 40)
        }));
        assert!(
            snapshot
                .changed_files(&DiffScope::Change("@".into()))
                .await?
                .iter()
                .any(|file| file.path == path && file.has_conflict)
        );
        let result = b"\xef\xbb\xbfheader\r\nresolved\nfooter";
        let prepared = snapshot
            .prepare_conflict_resolution(&conflict, &conflict.commit_id, Some(result), None)
            .await?;
        assert_eq!(prepared.snapshot().file_content("@", path).await?, result);
        assert_eq!(
            repository.snapshot(false).await?.operation(),
            snapshot.operation()
        );
        let publication = prepared.publish().await?;
        assert!(publication.synchronization_error.is_none());
        assert_eq!(fs::read(fixture.root().join(path))?, result);
        assert!(publication.snapshot.conflicted_paths("@").await?.is_empty());
        assert_eq!(
            publication
                .snapshot
                .file_content("@", "unselected.txt")
                .await?,
            b"unchanged\n"
        );
        fixture.run(&["undo"])?;
        let restored = repository.snapshot(false).await?;
        assert_eq!(
            restored.file_conflict("@", path).await?.materialized,
            conflict.materialized
        );

        let prepared = restored
            .prepare_conflict_resolution(&conflict, &conflict.commit_id, None, None)
            .await?;
        prepared.publish().await?;
        assert!(!fixture.root().join(path).exists());
        fixture.run(&["undo"])?;
        assert!(
            repository
                .snapshot(false)
                .await?
                .conflicted_paths("@")
                .await?
                .contains(&path.into())
        );
        Ok(())
    })
}

#[test]
fn conflict_resolution_rejects_external_rewrites_and_filters_inherited_conflicts() -> Result<()> {
    smol::block_on(async {
        let fixture = conflict_fixture(b"base\n", b"left\n", b"right\n", "file.txt")?;
        let repository = fixture.repository().await?;
        let snapshot = repository.snapshot(true).await?;
        let conflict = snapshot.file_conflict("@", "file.txt").await?;
        fs::write(fixture.root().join("unselected.txt"), b"later saved file\n")?;
        let current = repository.snapshot_before_write().await?;
        assert!(
            current
                .prepare_conflict_resolution(
                    &conflict,
                    &conflict.commit_id,
                    Some(b"resolved\n"),
                    None
                )
                .await
                .is_err()
        );
        let acknowledged = current.exact_revision("@").await?.full_commit_id;
        let prepared = current
            .prepare_conflict_resolution(&conflict, &acknowledged, Some(b"resolved\n"), None)
            .await?;
        drop(prepared);
        assert_eq!(
            repository.snapshot(false).await?.operation(),
            current.operation()
        );
        assert_eq!(
            current.file_content("@", "unselected.txt").await?,
            b"later saved file\n"
        );
        assert!(
            current
                .prepare_conflict_resolution(
                    &conflict,
                    &acknowledged,
                    Some(&conflict.materialized),
                    None
                )
                .await
                .is_err()
        );
        let inherited = fixture.run(&["new", "-m", "inherited conflict"])?;
        assert!(inherited.is_empty());
        let inherited = repository.snapshot(false).await?;
        assert!(inherited.exact_revision("@").await?.has_conflict);
        assert!(
            inherited
                .changed_files(&DiffScope::Change("@".into()))
                .await?
                .is_empty()
        );
        Ok(())
    })
}

fn draft_fixture() -> Result<ConflictDraft> {
    let inputs = [
        (Side, "左\r\n尾"),
        (Base, "common\r\nbase\r\nbetween\nbase2\nend\n"),
        (Side, "right\n"),
    ]
    .into_iter()
    .map(|(role, text)| ConflictInput {
        role,
        index: usize::from(role == Side && text == "right\n"),
        id: Some("a".repeat(40)),
        executable: false,
        bytes: text.as_bytes().into(),
        source: None,
    })
    .collect();
    ConflictDraft::open(FileConflict {
        repository_root: "/test".into(), path: "file.txt".into(), change_id: "k".repeat(32),
        commit_id: "a".repeat(40), immutable: false, inputs, marker_length: 7, binary: false,
        materialized: concat!(
            "\u{feff}common\r\n<<<<<<< conflict 1\r\n+++++++ left\r\n左\r\n------- base\r\nbase\r\n+++++++ right\r\nright\r\n>>>>>>> conflict 1\r\n",
            "between\n<<<<<<< conflict 2\n+++++++ left\n左2\n------- base\nbase2\n+++++++ right\nright2\n>>>>>>> conflict 2\nend\n"
        ).as_bytes().into(),
    })
}

#[test]
fn conflict_draft_acceptance_preserves_eol_and_native_undo_metadata() -> Result<()> {
    let mut draft = draft_fixture()?;
    let original = draft.text.clone();
    assert_eq!(
        draft.editor_text(),
        "common\n左\nbase\nright\nbetween\n左2\nbase2\nright2\nend\n"
    );
    assert_eq!(
        draft.line_numbers(),
        [
            Some(1),
            None,
            Some(2),
            None,
            Some(3),
            None,
            Some(4),
            None,
            Some(5),
            None
        ]
    );
    let mut text = draft.editor_text();
    let offset = text.find('左').context("Missing input")?;
    text.replace_range(offset..offset + '左'.len_utf8(), "右側");
    draft.update(&text, None, ConflictEditReason::Edit)?;
    assert!(!draft.conflicts[0].edited);
    assert!(draft.text.starts_with("\u{feff}common\r\n右側\r\n"));
    let edited = draft.text.clone();
    let starts = draft
        .conflicts
        .iter()
        .map(|conflict| conflict.range.start)
        .collect::<Vec<_>>();
    let acceptance = draft.prepare_accept(&starts, ConflictChoice::Input(Side, 0))?;
    draft.accept(acceptance)?;
    assert!(draft.conflicts.is_empty());
    assert_eq!(draft.text, "\u{feff}common\r\n右側\r\nbetween\n左2\nend\n");
    assert!(draft.line_numbers().iter().all(Option::is_some));
    let accepted = draft.text.clone();
    draft.update(
        &normalize_conflict_text(&edited),
        None,
        ConflictEditReason::Undo,
    )?;
    assert_eq!(draft.conflicts.len(), 2);
    assert_eq!(draft.text, edited);
    draft.update(
        &normalize_conflict_text(&original),
        None,
        ConflictEditReason::Undo,
    )?;
    assert_eq!(draft.text, original);
    draft.update(
        &normalize_conflict_text(&accepted),
        None,
        ConflictEditReason::Redo,
    )?;
    assert_eq!(draft.text, accepted);
    assert!(draft.conflicts.is_empty());
    draft.mark_saved();
    let restored: ConflictDraft = serde_json::from_slice(&serde_json::to_vec(&draft)?)?;
    assert_eq!(restored.text, draft.text);
    assert!(!restored.is_dirty());
    draft.mark_applied(&accepted, false);
    assert!(!draft.has_newer_edits());
    draft.update("newer result\n", None, ConflictEditReason::Edit)?;
    assert!(draft.has_newer_edits());
    Ok(())
}

#[test]
fn conflict_draft_cross_input_edits_require_explicit_confirmation() -> Result<()> {
    let mut draft = draft_fixture()?;
    let original = draft.editor_text();
    let mut text = original.clone();
    let block = draft.conflicts[0].range.clone();
    text.replace_range(block.clone(), "manually merged\n");
    draft.update(&text, None, ConflictEditReason::Edit)?;
    assert_eq!(draft.conflicts.len(), 2);
    assert!(draft.conflicts[0].edited);
    let starts = draft
        .conflicts
        .iter()
        .map(|conflict| conflict.range.start)
        .collect::<Vec<_>>();
    assert!(
        draft
            .prepare_accept(&starts, ConflictChoice::Input(Side, 0))
            .is_err()
    );
    assert_eq!(draft.conflicts.len(), 2);
    let confirmation = draft.prepare_accept(&[block.start], ConflictChoice::Edited)?;
    assert_eq!(confirmation.edits[0].text, "manually merged\n");
    draft.accept(confirmation)?;
    assert_eq!(draft.editor_text(), text);
    assert_eq!(draft.conflicts.len(), 1);
    draft.update(&original, None, ConflictEditReason::Undo)?;
    assert_eq!(draft.conflicts.len(), 2);
    assert!(!draft.conflicts[0].edited);
    Ok(())
}

#[test]
fn conflict_draft_empty_side_and_unterminated_inputs_are_not_implicitly_deleted() -> Result<()> {
    let mut conflict = draft_fixture()?.conflict;
    conflict.materialized =
        b"<<<<<<< conflict 1\n+++++++ left\n------- base\n+++++++ right\nlast\n>>>>>>> conflict 1"
            .to_vec();
    let mut draft = ConflictDraft::open(conflict)?;
    assert_eq!(draft.editor_text(), "\nlast\n");
    assert!(draft.conflicts[0].sections[1].is_empty_base());
    let mut right = draft.clone();
    right.accept(right.prepare_accept(&[0], ConflictChoice::Input(Side, 1))?)?;
    assert_eq!(right.text, "last");
    draft.accept(draft.prepare_accept(&[0], ConflictChoice::Input(Side, 0))?)?;
    assert_eq!(draft.text, "");
    assert!(!draft.applied_deleted);
    assert!(draft.conflicts.is_empty());
    Ok(())
}

#[cfg(unix)]
#[test]
fn binary_conflict_selection_preserves_bytes_and_executable_mode() -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    smol::block_on(async {
        let fixture = conflict_fixture(b"base\0\xff", b"left\0\xfe", b"right\0\xfd", "file.bin")?;
        let repository = fixture.repository().await?;
        let snapshot = repository.snapshot(true).await?;
        let conflict = snapshot.file_conflict("@", "file.bin").await?;
        assert!(conflict.binary);
        assert!(conflict.text().is_err());
        let selected = conflict
            .inputs
            .iter()
            .find(|input| input.bytes == b"right\0\xfd")
            .context("Missing binary version")?;
        snapshot
            .prepare_conflict_resolution(
                &conflict,
                &conflict.commit_id,
                Some(&selected.bytes),
                Some(true),
            )
            .await?
            .publish()
            .await?;
        assert_eq!(fs::read(fixture.root().join("file.bin"))?, selected.bytes);
        assert_ne!(
            fs::metadata(fixture.root().join("file.bin"))?
                .permissions()
                .mode()
                & 0o111,
            0
        );
        fixture.run(&["undo"])?;
        repository
            .snapshot(false)
            .await?
            .prepare_conflict_resolution(&conflict, &conflict.commit_id, Some(b""), Some(false))
            .await?
            .publish()
            .await?;
        assert_eq!(fs::read(fixture.root().join("file.bin"))?, b"");
        assert_eq!(
            fs::metadata(fixture.root().join("file.bin"))?
                .permissions()
                .mode()
                & 0o111,
            0
        );
        Ok(())
    })
}
