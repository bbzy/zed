use crate::{
    ChangeKind, ChangedFile, DiffScope, Repository, Snapshot,
    repository::{literal_fileset, parse_records, validate_path},
};
use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use smol::{io::AsyncReadExt as _, process::Command};
use std::{collections::BTreeSet, path::PathBuf, process::Stdio};

#[derive(Debug)]
pub enum SparseCheckout {
    Ready(Snapshot),
    Cancelled,
    PendingSavedInputs,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct BlameLine {
    pub change_id: String,
    pub full_change_id: String,
    #[serde(default)]
    pub change_id_prefix: String,
    pub commit_id: String,
    pub author: String,
    pub date: String,
    pub summary: String,
    pub first: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct LfsPointer {
    pub hash: String,
    pub size: u64,
}

impl LfsPointer {
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        let text = std::str::from_utf8(bytes).ok()?;
        let mut lines = text.lines();
        if lines.next()? != "version https://git-lfs.github.com/spec/v1" {
            return None;
        }

        let hash = lines.next()?.strip_prefix("oid sha256:")?;
        if hash.len() != 64
            || !hash
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return None;
        }

        let size = lines.next()?.strip_prefix("size ")?;
        if size.is_empty()
            || !size.bytes().all(|byte| byte.is_ascii_digit())
            || lines.next().is_some()
        {
            return None;
        }

        let size = size.parse().ok()?;
        if size > 9_007_199_254_740_991 {
            return None;
        }

        Some(Self {
            hash: hash.into(),
            size,
        })
    }

    pub fn text(&self) -> String {
        format!(
            "version https://git-lfs.github.com/spec/v1\noid sha256:{}\nsize {}\n",
            self.hash, self.size
        )
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FileInformation {
    pub path: String,
    pub revision: String,
    pub exists: Option<bool>,
    pub file_type: Option<String>,
    pub size: Option<u64>,
    pub hash: Option<String>,
    pub modified: Option<String>,
    pub commit_time: Option<String>,
    pub object_id: Option<String>,
    pub pointer: Option<LfsPointer>,
    pub unavailable: Option<String>,
}

impl FileInformation {
    fn new(path: &str, revision: &str) -> Self {
        Self {
            path: path.into(),
            revision: revision.into(),
            exists: None,
            file_type: None,
            size: None,
            hash: None,
            modified: None,
            commit_time: None,
            object_id: None,
            pointer: None,
            unavailable: None,
        }
    }

    fn contents(&mut self, bytes: &[u8]) {
        self.pointer = LfsPointer::parse(bytes);
        self.size = Some(
            self.pointer
                .as_ref()
                .map_or(bytes.len() as u64, |pointer| pointer.size),
        );
        self.hash = Some(self.pointer.as_ref().map_or_else(
            || format!("{:x}", Sha256::digest(bytes)),
            |pointer| pointer.hash.clone(),
        ));
    }

    pub fn format(&self) -> String {
        let value = |value: &Option<String>| value.clone().unwrap_or_else(|| "Unavailable".into());
        let mut lines = vec![
            format!("File: {}", self.path),
            format!("Revision: {}", self.revision),
            format!(
                "Exists: {}",
                match self.exists {
                    Some(true) => "Yes",
                    Some(false) => "No",
                    None => "Unknown",
                }
            ),
            format!("Type: {}", value(&self.file_type)),
            format!(
                "Size: {}",
                self.size
                    .map_or_else(|| "Unavailable".into(), |size| format!("{size} bytes"))
            ),
            format!("SHA-256: {}", value(&self.hash)),
            format!("Modified: {}", value(&self.modified)),
        ];
        if let Some(time) = &self.commit_time {
            lines.push(format!("Version commit time: {time}"));
        }

        if let Some(object) = &self.object_id {
            lines.push(format!("Repository object: {object}"));
        }

        if self.pointer.is_some() {
            lines.push("Representation: LFS pointer (hash and size refer to its content)".into());
        }

        if let Some(error) = &self.unavailable {
            lines.push(format!("Content: {error}"));
        }

        lines.push(String::new());
        lines.join("\n")
    }

    pub fn pointer_text(&self) -> Option<String> {
        if self.exists == Some(false) {
            return Some(String::new());
        }

        Some(
            LfsPointer {
                hash: self.hash.clone()?,
                size: self.size?,
            }
            .text(),
        )
    }
}

impl Snapshot {
    pub async fn merged_parent_file(
        &self,
        revision: &str,
        file: &ChangedFile,
    ) -> Result<(FileInformation, Option<Vec<u8>>)> {
        let original_path = file.renamed_from.as_deref().unwrap_or(&file.path);
        validate_path(original_path)?;
        validate_path(&file.path)?;
        let mut information =
            FileInformation::new(original_path, &format!("Merged parents of {revision}"));
        information.exists = Some(file.change_type != ChangeKind::Added);
        if information.exists == Some(false) {
            return Ok((information, None));
        }

        // The merged parent tree has no commit ID. Reverse KK's own patch rather
        // than approximating that tree with one of the merge's parents.
        let contents = async {
            let modified = self
                .file_content_if_exists(revision, &file.path)
                .await?
                .unwrap_or_default();
            let patch = self
                .diff(&DiffScope::Change(revision.into()), Some(&file.path))
                .await?;
            ensure!(
                !patch
                    .split(|byte| *byte == b'\n')
                    .any(|line| line.starts_with(b"Binary files ")),
                "KK does not include binary contents in its patch for the merged parent tree"
            );
            // KK emits unquoted paths; reconstruction only needs the hunk bytes.
            let hunks = patch
                .split_inclusive(|byte| *byte == b'\n')
                .skip_while(|line| !line.starts_with(b"@@ "))
                .flatten()
                .copied()
                .collect::<Vec<_>>();
            let patch = diffy::Patch::from_bytes(&hunks)
                .context("Unable to read the merged parent patch")?;
            diffy::apply_bytes(&modified, &patch.reverse())
                .context("Unable to reconstruct the merged parent contents")
        }
        .await;

        match contents {
            Ok(contents) => {
                information.file_type = Some("file".into());
                information.contents(&contents);
                Ok((information, Some(contents)))
            }
            Err(error) => {
                information.unavailable = Some(format!("{error:#}"));
                Ok((information, None))
            }
        }
    }

    pub async fn annotate(&self, revision: &str, path: &str) -> Result<Vec<BlameLine>> {
        if let Some(remote) = self.repository().remote() {
            return remote
                .read(
                    self,
                    crate::remote::Read::Annotate {
                        revision: revision.into(),
                        path: path.into(),
                    },
                )
                .await;
        }

        validate_path(path)?;
        let template = r#""{\"change_id\":" ++ json(stringify(commit.change_id().shortest(8))) ++ ",\"change_id_prefix\":" ++ json(commit.change_id().shortest(8).prefix()) ++ ",\"full_change_id\":" ++ json(commit.change_id()) ++ ",\"commit_id\":" ++ json(commit.commit_id()) ++ ",\"author\":" ++ json(commit.author().name()) ++ ",\"date\":" ++ json(commit.author().timestamp()) ++ ",\"summary\":" ++ json(commit.description().first_line()) ++ ",\"first\":" ++ json(first_line_in_hunk) ++ "}\n""#;
        parse_records(
            &self
                .read(&[
                    "file".into(),
                    "annotate".into(),
                    "-r".into(),
                    revision.into(),
                    "-T".into(),
                    template.into(),
                    "--".into(),
                    path.into(),
                ])
                .await?,
        )
    }

    pub async fn file_information(&self, revision: &str, path: &str) -> Result<FileInformation> {
        if let Some(remote) = self.repository().remote() {
            return remote
                .read(
                    self,
                    crate::remote::Read::FileInformation {
                        revision: revision.into(),
                        path: path.into(),
                    },
                )
                .await;
        }

        #[derive(Deserialize)]
        struct Entry {
            kind: String,
            object: String,
        }
        #[derive(Deserialize)]
        struct Metadata {
            commit: String,
            time: String,
            entries: Vec<Entry>,
        }
        let fileset = serde_json::to_string(&literal_fileset(path)?)?;
        let template = format!(
            r#""{{\"commit\":" ++ json(commit_id) ++ ",\"time\":" ++ json(committer.timestamp().format("%Y-%m-%dT%H:%M:%S%z")) ++ ",\"entries\":[" ++ self.files({fileset}).map(|file| "{{\"kind\":" ++ json(file.file_type()) ++ ",\"object\":\"" ++ file.normal_value() ++ "\"}}").join(",") ++ "]}}""#
        );
        let metadata = self
            .read(&[
                "log".into(),
                "-r".into(),
                revision.into(),
                "--no-graph".into(),
                "-T".into(),
                template,
            ])
            .await?;
        let metadata: Metadata =
            serde_json::from_slice(&metadata).context("Invalid file information from KK")?;
        let mut information = FileInformation::new(path, &metadata.commit);
        information.commit_time = Some(metadata.time);
        information.modified = Some("Not recorded for historical files".into());
        information.exists = Some(!metadata.entries.is_empty());
        if let Some(entry) = metadata.entries.first() {
            information.file_type = Some(entry.kind.clone());
            information.object_id = (!entry.object.is_empty()).then(|| entry.object.clone());
            if entry.kind == "file" {
                match self.file_content(revision, path).await {
                    Ok(bytes) => information.contents(&bytes),
                    Err(error) => information.unavailable = Some(format!("{error:#}")),
                }
            }
        }

        Ok(information)
    }
}

impl Repository {
    pub async fn excluded_sparse_files(&self, paths: &[String]) -> Result<Vec<String>> {
        if let Some(remote) = self.remote() {
            return remote
                .data(crate::remote::Request::ExcludedSparseFiles {
                    repository: remote.id,
                    paths: paths.to_vec(),
                })
                .await;
        }

        let mut missing = BTreeSet::new();
        for path in paths {
            if !self.working_file_exists(path).await? {
                missing.insert(path.clone());
            }
        }

        if missing.is_empty() {
            return Ok(Vec::new());
        }

        let patterns = self
            .command(&["sparse", "list", "--ignore-working-copy"])
            .await?
            .stdout;
        let patterns =
            std::str::from_utf8(&patterns).context("KK sparse patterns are not UTF-8")?;
        let snapshot = self.snapshot_unlocked(false).await?;
        let mut excluded = Vec::new();
        for path in missing {
            if patterns.lines().any(|pattern| {
                pattern == "."
                    || path == pattern
                    || path
                        .strip_prefix(pattern)
                        .is_some_and(|suffix| suffix.starts_with('/'))
            }) {
                continue;
            }

            let tracked = snapshot
                .read(&[
                    "file".into(),
                    "list".into(),
                    "-r".into(),
                    "@".into(),
                    "--".into(),
                    literal_fileset(&path)?,
                ])
                .await?;
            if !tracked.is_empty() {
                excluded.push(path);
            }
        }

        Ok(excluded)
    }

    pub async fn expand_sparse_checkout(
        &self,
        paths: &[String],
        is_current: impl Fn() -> bool + Send + Sync,
    ) -> Result<SparseCheckout> {
        if let Some(remote) = self.remote() {
            return remote.sparse(self, paths, is_current).await;
        }

        let _write_guard = crate::transaction::write_lock(self.root()).await?;
        if !is_current() {
            return Ok(SparseCheckout::Cancelled);
        }

        self.check_lifetime()?;
        if crate::saved::is_deferred(self.root())? {
            return Ok(SparseCheckout::PendingSavedInputs);
        }

        let remaining = self.excluded_sparse_files(paths).await?;
        if !is_current() {
            return Ok(SparseCheckout::Cancelled);
        }

        self.check_lifetime()?;
        if crate::saved::is_deferred(self.root())? {
            return Ok(SparseCheckout::PendingSavedInputs);
        }

        if !remaining.is_empty() {
            let mut arguments = vec!["sparse".to_owned(), "set".into()];
            arguments.extend(remaining.iter().map(|path| format!("--add={path}")));
            self.command(&arguments.iter().map(String::as_str).collect::<Vec<_>>())
                .await?;
        }

        for path in paths {
            ensure!(
                self.working_file_exists(path).await?,
                "{path} is still unavailable after updating the sparse checkout"
            );
        }

        Ok(SparseCheckout::Ready(self.snapshot_unlocked(false).await?))
    }

    async fn working_file_exists(&self, path: &str) -> Result<bool> {
        validate_path(path)?;
        match smol::fs::metadata(self.root().join(path)).await {
            Ok(_) => Ok(true),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                Ok(false)
            }
            Err(error) => Err(error.into()),
        }
    }

    pub(crate) async fn internal_git_root(&self) -> Result<PathBuf> {
        let marker = self.root().join(".kk/repo");
        let repository = if smol::fs::metadata(&marker).await?.is_dir() {
            marker
        } else {
            self.root()
                .join(".kk")
                .join(smol::fs::read_to_string(marker).await?)
        };
        let store = smol::fs::canonicalize(repository.join("store")).await?;
        let target = smol::fs::read_to_string(store.join("git_target")).await?;
        let git_root = smol::fs::canonicalize(store.join(target)).await?;
        ensure!(
            git_root == store.join("git"),
            "KK requires internal bare Git storage"
        );
        let output = Command::new("git")
            .arg("--git-dir")
            .arg(&git_root)
            .args(["rev-parse", "--is-bare-repository"])
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output()
            .await?;
        ensure!(
            output.status.success() && output.stdout == b"true\n",
            "KK requires internal bare Git storage"
        );
        Ok(git_root)
    }

    pub async fn lfs_content(&self, pointer: &LfsPointer) -> Result<Vec<u8>> {
        if let Some(remote) = self.remote() {
            return remote
                .data(crate::remote::Request::LfsContent {
                    repository: remote.id,
                    pointer: pointer.clone(),
                })
                .await;
        }

        ensure!(
            pointer.hash.len() == 64
                && pointer
                    .hash
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
            "Invalid LFS object ID"
        );
        let git_root = self.internal_git_root().await?;
        let output = Command::new("git")
            .arg("--git-dir")
            .arg(&git_root)
            .args(["config", "--path", "--get", "lfs.storage"])
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output()
            .await?;
        ensure!(
            output.status.success() || output.status.code() == Some(1),
            "Could not read LFS storage: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let configured = std::str::from_utf8(&output.stdout)?.trim();
        let storage = if configured.is_empty() {
            git_root.join("lfs")
        } else {
            git_root.join(configured)
        };
        let object = storage
            .join("objects")
            .join(&pointer.hash[..2])
            .join(&pointer.hash[2..4])
            .join(&pointer.hash);
        let bytes = smol::fs::read(object).await.context(
            "LFS content is not available locally; fetch the content and reopen the diff",
        )?;
        ensure!(
            bytes.len() as u64 == pointer.size
                && format!("{:x}", Sha256::digest(&bytes)) == pointer.hash,
            "Invalid local LFS content for {}",
            pointer.hash
        );
        Ok(bytes)
    }

    pub async fn working_file_content(&self, path: &str) -> Result<Vec<u8>> {
        if let Some(remote) = self.remote() {
            return remote
                .data(crate::remote::Request::WorkingFile {
                    repository: remote.id,
                    path: path.into(),
                })
                .await;
        }

        validate_path(path)?;
        Ok(smol::fs::read(self.root().join(path)).await?)
    }

    pub async fn working_file_information(
        &self,
        path: &str,
        draft: Option<&[u8]>,
    ) -> Result<FileInformation> {
        if let Some(remote) = self.remote() {
            return remote
                .data(crate::remote::Request::WorkingFileInformation {
                    repository: remote.id,
                    path: path.into(),
                    draft: draft.map(<[u8]>::to_vec),
                })
                .await;
        }

        validate_path(path)?;
        let mut information = FileInformation::new(path, "Working copy");
        if let Some(draft) = draft {
            information.exists = Some(true);
            information.file_type = Some("File (unsaved editor contents)".into());
            information.modified = Some("Unsaved editor changes".into());
            information.contents(draft);
            return Ok(information);
        }

        let absolute = self.root().join(path);
        let metadata = match smol::fs::metadata(&absolute).await {
            Ok(metadata) => metadata,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                information.exists = Some(false);
                return Ok(information);
            }
            Err(error) => {
                information.unavailable = Some(error.to_string());
                return Ok(information);
            }
        };
        information.exists = Some(true);
        information.file_type = Some(
            if metadata.is_file() {
                "File"
            } else if metadata.is_dir() {
                "Directory"
            } else {
                "Other"
            }
            .into(),
        );
        information.size = Some(metadata.len());
        information.modified = Some(
            time::OffsetDateTime::from(metadata.modified()?)
                .format(&time::format_description::well_known::Rfc3339)?,
        );
        if metadata.is_file() {
            let contents = async {
                let mut file = smol::fs::File::open(&absolute).await?;
                let before = file.metadata().await?;
                let mut hash = Sha256::new();
                let mut prefix = Vec::new();
                let mut chunk = [0; 64 * 1024];
                loop {
                    let length = file.read(&mut chunk).await?;
                    if length == 0 {
                        break;
                    }

                    hash.update(&chunk[..length]);
                    if before.len() <= 1024 {
                        let prefix_length = length.min(1024_usize.saturating_sub(prefix.len()));
                        prefix.extend_from_slice(&chunk[..prefix_length]);
                    }
                }

                let after = file.metadata().await?;
                anyhow::Ok((before, after, hash, prefix))
            }
            .await;
            match contents {
                Ok((before, after, hash, prefix)) => {
                    if before.len() != after.len() || before.modified()? != after.modified()? {
                        information.unavailable = Some("File changed while reading".into());
                    } else {
                        information.size = Some(after.len());
                        information.modified = Some(
                            time::OffsetDateTime::from(after.modified()?)
                                .format(&time::format_description::well_known::Rfc3339)?,
                        );
                        information.hash = Some(format!("{:x}", hash.finalize()));
                        if after.len() <= 1024 {
                            information.contents(&prefix);
                        }
                    }
                }
                Err(error) => information.unavailable = Some(error.to_string()),
            }
        }

        Ok(information)
    }
}
