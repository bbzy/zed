use super::*;

#[test]
fn diffedit_preserves_other_paths_and_rebases_descendants() -> Result<()> {
    smol::block_on(async {
        for replacement in [b"edited\r\nlast".as_slice(), b"".as_slice()] {
            let fixture = Fixture::create()?;
            let path = "-quoted [file].txt";
            fs::write(fixture.root().join(path), b"original\r\nlast")?;
            fs::write(fixture.root().join("other.txt"), b"other\n")?;
            fixture.run(&["new", "-m", "descendant"])?;
            fs::write(fixture.root().join("descendant.txt"), b"descendant\n")?;
            let repository = fixture.repository().await?;
            let snapshot = repository.snapshot(true).await?;
            let mut edit = snapshot.file_edit("@-", path).await?;
            edit.contents = replacement.to_vec();
            let target = edit.revision.revset();
            let before = snapshot.exact_revision("@").await?;
            let prepared = snapshot.prepare_file_edit(&edit).await?;
            assert_eq!(fs::read(fixture.root().join(path))?, edit.original);
            let publication = prepared.publish().await?;
            assert!(publication.synchronization_error.is_none());
            assert_eq!(
                publication.snapshot.file_content(&target, path).await?,
                replacement
            );
            assert_eq!(fs::read(fixture.root().join(path))?, replacement);
            assert_eq!(fs::read(fixture.root().join("other.txt"))?, b"other\n");
            assert_eq!(
                fs::read(fixture.root().join("descendant.txt"))?,
                b"descendant\n"
            );
            let after = publication.snapshot.exact_revision("@").await?;
            assert_eq!(before.full_change_id, after.full_change_id);
            assert_ne!(before.full_commit_id, after.full_commit_id);
        }
        anyhow::Ok(())
    })
}

#[test]
fn diffedit_rejects_stale_targets_and_unsupported_files() -> Result<()> {
    smol::block_on(async {
        let fixture = Fixture::create()?;
        fs::write(fixture.root().join("file.txt"), b"original\n")?;
        fs::write(fixture.root().join("binary.txt"), b"binary\0data")?;
        fixture.run(&["new"])?;
        let repository = fixture.repository().await?;
        let snapshot = repository.snapshot(true).await?;
        assert!(snapshot.file_edit("@", "file.txt").await.is_err());
        assert!(snapshot.file_edit("@-", "binary.txt").await.is_err());
        assert!(snapshot.file_edit("root()", "file.txt").await.is_err());
        let mut edit = snapshot.file_edit("@-", "file.txt").await?;
        edit.contents = b"my draft\n".to_vec();
        fixture.run(&["describe", "-r", "@-", "-m", "external rewrite"])?;
        let current = repository.snapshot(true).await?;
        assert!(current.prepare_file_edit(&edit).await.is_err());
        assert_eq!(fs::read(fixture.root().join("file.txt"))?, b"original\n");
        let mut edit = current.file_edit("@-", "file.txt").await?;
        edit.path = "../outside.txt".into();
        assert!(current.prepare_file_edit(&edit).await.is_err());
        anyhow::Ok(())
    })
}

#[test]
fn diffedit_preserves_executable_bit() -> Result<()> {
    #[cfg(unix)]
    smol::block_on(async {
        use std::os::unix::fs::PermissionsExt as _;
        let fixture = Fixture::create()?;
        let path = fixture.root().join("file.sh");
        fs::write(&path, b"echo before\n")?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
        fixture.run(&["new"])?;
        let repository = fixture.repository().await?;
        let snapshot = repository.snapshot(true).await?;
        let mut edit = snapshot.file_edit("@-", "file.sh").await?;
        edit.contents = b"echo after\n".to_vec();
        snapshot.prepare_file_edit(&edit).await?.publish().await?;
        assert_ne!(fs::metadata(&path)?.permissions().mode() & 0o111, 0);
        anyhow::Ok(())
    })?;
    Ok(())
}
