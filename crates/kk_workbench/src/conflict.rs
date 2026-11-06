use crate::{
    DiffScope, Publication, Snapshot, repository::literal_fileset, transaction::Transaction,
};
use anyhow::{Context as _, Result, ensure};
use regex::Regex;
use serde::{Deserialize, Serialize};
use smol::process::Command;
use std::{collections::HashMap, ops::Range, path::PathBuf, process::Stdio};

const SNAPSHOT_CONFIG: &str = "ui.conflict-marker-style=\"snapshot\"";
const CONFLICT_PATHS: &str = "json(conflicted_files.map(|file| stringify(file.path())))";

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum ConflictRole {
    Side,
    Base,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ConflictSource {
    pub change_id: String,
    #[serde(default)]
    pub display_change_id: Option<String>,
    #[serde(default)]
    pub change_id_prefix: String,
    pub commit_id: String,
    pub description: String,
    pub bookmarks: Vec<String>,
    pub role: Option<String>,
    pub details_incomplete: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ConflictInput {
    pub role: ConflictRole,
    pub index: usize,
    pub id: Option<String>,
    pub executable: bool,
    pub bytes: Vec<u8>,
    pub source: Option<ConflictSource>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FileConflict {
    pub repository_root: PathBuf,
    pub path: String,
    pub change_id: String,
    pub commit_id: String,
    pub immutable: bool,
    pub inputs: Vec<ConflictInput>,
    pub materialized: Vec<u8>,
    pub marker_length: usize,
    pub binary: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConflictSection {
    pub role: ConflictRole,
    pub index: usize,
    pub label: String,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConflictHunk {
    pub range: Range<usize>,
    pub sections: Vec<ConflictSection>,
}

pub struct PreparedConflictResolution {
    transaction: crate::transaction::PreparedTransaction,
    target: String,
}

impl ConflictInput {
    pub fn same_base(&self, other: &Self) -> bool {
        self.role == ConflictRole::Base
            && other.role == ConflictRole::Base
            && self.id.is_some() == other.id.is_some()
            && self.executable == other.executable
            && self.bytes == other.bytes
    }

    pub fn title(&self, base_count: usize) -> String {
        if self.role == ConflictRole::Base {
            return if base_count == 1 {
                "Base · original".into()
            } else {
                format!("Base {}", self.index + 1)
            };
        }

        let Some(source) = &self.source else {
            return "Unknown source".into();
        };
        let mut title = if source.bookmarks.is_empty() {
            let mut title: String = source
                .display_change_id
                .as_ref()
                .unwrap_or(&source.change_id)
                .clone();
            let description = source
                .description
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            if !description.is_empty() {
                title.push_str(" · ");
                if description.chars().count() > 48 {
                    title.extend(description.chars().take(47));
                    title.push('…');
                } else {
                    title.push_str(&description);
                }
            }

            title
        } else {
            source.bookmarks.join(", ")
        };
        if let Some(role) = &source.role {
            title.push_str(&format!(" ({role})"));
        }

        title
    }
}

impl FileConflict {
    pub fn revset(&self) -> String {
        format!("exactly(change_id({}), 1)", self.change_id)
    }

    pub fn text(&self) -> Result<&str> {
        ensure!(
            !self.binary,
            "Choose a whole version to resolve this binary conflict"
        );
        Ok(std::str::from_utf8(&self.materialized)?)
    }
}

impl PreparedConflictResolution {
    pub fn snapshot(&self) -> &Snapshot {
        self.transaction.snapshot()
    }

    pub(crate) async fn receipt(&self) -> Result<crate::OperationReceipt> {
        self.transaction
            .file_change_receipt(std::slice::from_ref(&self.target))
            .await
    }

    pub async fn publish(self) -> Result<Publication> {
        self.transaction.publish_file_change(&[self.target]).await
    }
}

impl Snapshot {
    pub async fn conflicted_paths(&self, revision: &str) -> Result<Vec<String>> {
        if let Some(remote) = self.repository().remote() {
            return remote
                .read(
                    self,
                    crate::remote::Read::ConflictedPaths {
                        revision: revision.into(),
                    },
                )
                .await;
        }

        let bytes = self
            .read(&[
                "log".into(),
                "-r".into(),
                format!("exactly({revision}, 1)"),
                "--no-graph".into(),
                "-T".into(),
                CONFLICT_PATHS.into(),
            ])
            .await?;
        let paths: Vec<String> = serde_json::from_slice(&bytes)?;
        for path in &paths {
            crate::repository::validate_path(path)?;
        }

        Ok(paths)
    }

    pub async fn file_conflict(&self, revision: &str, path: &str) -> Result<FileConflict> {
        if let Some(remote) = self.repository().remote() {
            return remote
                .read(
                    self,
                    crate::remote::Read::FileConflict {
                        revision: revision.into(),
                        path: path.into(),
                    },
                )
                .await;
        }

        validate_conflict_path(path)?;
        let revision = self.exact_revision(revision).await?;
        let immutable = self
            .read(&[
                "log".into(),
                "-r".into(),
                revision.full_commit_id.clone(),
                "--no-graph".into(),
                "-T".into(),
                "immutable".into(),
            ])
            .await?;
        ensure!(
            matches!(immutable.as_slice(), b"true" | b"false"),
            "Invalid immutable revision metadata"
        );
        let raw = self
            .read(&[
                "debug".into(),
                "tree".into(),
                "-r".into(),
                revision.full_commit_id.clone(),
                "--".into(),
                literal_fileset(path)?,
            ])
            .await?;
        let mut inputs = parse_conflict_inputs(std::str::from_utf8(&raw)?, path)?;
        let git_root = self.repository().internal_git_root().await?;
        let mut blobs = HashMap::<String, Vec<u8>>::new();
        for input in &mut inputs {
            let Some(id) = &input.id else { continue };
            if let Some(bytes) = blobs.get(id) {
                input.bytes = bytes.clone();
                continue;
            }

            let size = Command::new("git")
                .arg("--git-dir")
                .arg(&git_root)
                .args(["cat-file", "-s", id])
                .stdin(Stdio::null())
                .kill_on_drop(true)
                .output()
                .await?;
            ensure!(
                size.status.success(),
                "Cannot read a conflict input from the KK Git store: {}",
                String::from_utf8_lossy(&size.stderr)
            );
            let size: usize = std::str::from_utf8(&size.stdout)?.trim().parse()?;
            ensure!(size <= 50 * 1024 * 1024, "Conflict input exceeds 50 MiB");
            let output = Command::new("git")
                .arg("--git-dir")
                .arg(&git_root)
                .args(["cat-file", "blob", id])
                .stdin(Stdio::null())
                .kill_on_drop(true)
                .output()
                .await?;
            ensure!(
                output.status.success() && output.stdout.len() == size,
                "Cannot read a complete conflict input from the KK Git store: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            input.bytes = output.stdout;
            blobs.insert(id.clone(), input.bytes.clone());
        }

        let materialized = self
            .snapshot_conflict_content(&revision.full_commit_id, path)
            .await?;
        let binary = inputs
            .iter()
            .any(|input| input.bytes.contains(&0) || std::str::from_utf8(&input.bytes).is_err());
        let marker_length = conflict_marker_length(&inputs);
        // Binary snapshots are parsed only for ASCII source labels, never displayed as text.
        let text = if binary {
            materialized.iter().map(|byte| char::from(*byte)).collect()
        } else {
            ensure!(
                !materialized.contains(&0),
                "Invalid text conflict materialization"
            );
            String::from_utf8(materialized.clone())?
        };
        let hunks = parse_conflict_hunks(&text, marker_length)?;
        ensure!(
            binary
                || (!hunks.is_empty()
                    && hunks.iter().all(|hunk| hunk.sections.len() == inputs.len())),
            "This conflict cannot be represented as a text merge with the configured KK version"
        );
        self.load_conflict_sources(&mut inputs, &hunks).await;

        Ok(FileConflict {
            repository_root: self.repository().root().to_owned(),
            path: path.into(),
            change_id: revision.full_change_id,
            commit_id: revision.full_commit_id,
            immutable: immutable == b"true",
            inputs,
            materialized,
            marker_length,
            binary,
        })
    }

    pub async fn refresh_conflict_sources(&self, conflict: &mut FileConflict) -> Result<()> {
        if let Some(remote) = self.repository().remote() {
            *conflict = remote
                .read(
                    self,
                    crate::remote::Read::RefreshConflictSources {
                        conflict: conflict.clone(),
                    },
                )
                .await?;
            return Ok(());
        }

        ensure!(
            self.repository().root() == conflict.repository_root,
            "The selected repository changed"
        );
        let bytes = self
            .snapshot_conflict_content(&conflict.commit_id, &conflict.path)
            .await?;
        let text = if conflict.binary {
            bytes.iter().map(|byte| char::from(*byte)).collect()
        } else {
            String::from_utf8(bytes)?
        };
        let hunks = parse_conflict_hunks(&text, conflict.marker_length)?;
        self.load_conflict_sources(&mut conflict.inputs, &hunks)
            .await;
        Ok(())
    }

    async fn snapshot_conflict_content(&self, revision: &str, path: &str) -> Result<Vec<u8>> {
        self.read(&[
            "--config".into(),
            SNAPSHOT_CONFIG.into(),
            "file".into(),
            "show".into(),
            "-r".into(),
            revision.into(),
            "--".into(),
            literal_fileset(path)?,
        ])
        .await
    }

    async fn load_conflict_sources(&self, inputs: &mut [ConflictInput], hunks: &[ConflictHunk]) {
        for input in inputs {
            let labels: Vec<_> = hunks
                .iter()
                .filter_map(|hunk| {
                    hunk.sections
                        .iter()
                        .find(|section| section.role == input.role && section.index == input.index)
                        .map(|section| section.label.as_str())
                })
                .collect();
            let Some(label) = labels.first().filter(|label| {
                labels.len() == hunks.len() && labels.iter().all(|other| *other == **label)
            }) else {
                continue;
            };
            let mut words = label.split_whitespace();
            let (Some(change_id), Some(commit_id)) = (words.next(), words.next()) else {
                continue;
            };
            if change_id.len() < 4
                || !change_id.bytes().all(|byte| (b'k'..=b'z').contains(&byte))
                || !(4..=64).contains(&commit_id.len())
                || !commit_id
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            {
                continue;
            }

            let role = [
                "rebase destination",
                "rebased revision",
                "parents of rebased revision",
            ]
            .into_iter()
            .find(|role| label.contains(&format!("({role})")))
            .map(str::to_owned);
            let mut source = ConflictSource {
                change_id: change_id.into(),
                display_change_id: None,
                change_id_prefix: String::new(),
                commit_id: commit_id.into(),
                description: String::new(),
                bookmarks: Vec::new(),
                role,
                details_incomplete: true,
            };
            let details = async {
                let revision = self.exact_revision(commit_id).await?;
                ensure!(
                    revision.full_change_id.starts_with(change_id),
                    "Conflict source label does not match its snapshot"
                );
                source.display_change_id = Some(revision.change_id.clone());
                source.change_id_prefix = revision.change_id_prefix.clone();
                source.change_id = revision.full_change_id.clone();
                source.commit_id = revision.full_commit_id.clone();
                source.description = revision.full_description.clone();
                source.bookmarks = if revision.bookmarks.is_empty() {
                    revision.remote_bookmarks().collect()
                } else {
                    revision.bookmarks.clone()
                };
                let visible = self.revisions(Some(&revision.revset()), 2).await?;
                if let [current] = visible.as_slice() {
                    source.bookmarks = if current.bookmarks.is_empty() {
                        current.remote_bookmarks().collect()
                    } else {
                        current.bookmarks.clone()
                    };
                }

                source.bookmarks.sort();
                source.bookmarks.dedup();
                source.details_incomplete = false;
                anyhow::Ok(())
            }
            .await;
            if let Err(error) = details {
                log::warn!(
                    "Could not read KK conflict source details; retaining snapshot identity: {error:#}"
                );
            }

            if input
                .source
                .as_ref()
                .is_none_or(|previous| !source.details_incomplete || previous.details_incomplete)
            {
                input.source = Some(source);
            }
        }
    }

    pub async fn prepare_conflict_resolution(
        &self,
        conflict: &FileConflict,
        acknowledged_commit: &str,
        result: Option<&[u8]>,
        executable: Option<bool>,
    ) -> Result<PreparedConflictResolution> {
        if let Some(remote) = self.repository().remote() {
            let prepared = remote
                .prepare(
                    self,
                    crate::remote::Mutation::ResolveConflict {
                        conflict: conflict.clone(),
                        acknowledged_commit: acknowledged_commit.into(),
                        result: result.map(<[u8]>::to_vec),
                        executable,
                    },
                )
                .await?
                .context("Missing remote conflict resolution")?;
            return Ok(PreparedConflictResolution {
                transaction: crate::transaction::PreparedTransaction::Remote(prepared),
                target: conflict.revset(),
            });
        }

        validate_conflict_path(&conflict.path)?;
        ensure!(
            self.repository().root() == conflict.repository_root,
            "The selected repository changed"
        );
        ensure!(
            !conflict.immutable,
            "This change is immutable. Create a mutable change to resolve its conflict."
        );
        if !conflict.binary
            && let Some(result) = result
        {
            ensure!(
                !result.contains(&0),
                "Text conflict results must be UTF-8 without NUL bytes"
            );
            ensure!(
                parse_conflict_hunks(std::str::from_utf8(result)?, conflict.marker_length)?
                    .is_empty(),
                "Resolve every conflict block before applying"
            );
        }

        let mut transaction = Transaction::begin(self).await?;
        let target = conflict.revset();
        let current = self.exact_revision(&target).await?;
        ensure!(
            current.full_commit_id == acknowledged_commit,
            "This change changed since the conflict editor opened. Reopen the file; your draft is retained."
        );
        if current.full_commit_id != conflict.commit_id {
            ensure!(
                self.diff(
                    &DiffScope::Comparison {
                        base: conflict.commit_id.clone(),
                        target: current.full_commit_id.clone()
                    },
                    Some(&conflict.path)
                )
                .await?
                .is_empty(),
                "The selected conflict changed while this action was pending. Reopen the file; your draft is retained."
            );
        }

        let actual = self
            .snapshot_conflict_content(&current.full_commit_id, &conflict.path)
            .await?;
        ensure!(
            actual == conflict.materialized,
            "The selected conflict changed; your draft is retained"
        );

        let directory = smol::unblock(|| {
            tempfile::Builder::new()
                .prefix("zed-kk-resolution-")
                .tempdir()
        })
        .await?;
        let prepared = async {
            let expected = directory.path().join("expected");
            let replacement = directory.path().join("result");
            let validated = directory.path().join("validated");
            let script = directory.path().join("resolve.sh");
            smol::fs::write(&expected, &conflict.materialized).await?;
            smol::fs::write(&replacement, result.unwrap_or_default()).await?;
            smol::fs::write(&script, RESOLUTION_EDITOR).await?;
            let arguments = serde_json::to_string(&[
                script.to_str().context("Temporary path is not UTF-8")?, "$right", &conflict.path,
                expected.to_str().context("Temporary path is not UTF-8")?, replacement.to_str().context("Temporary path is not UTF-8")?,
                if result.is_none() {
                    "delete"
                } else {
                    "write"
                },
                match executable { Some(true) => "executable", Some(false) => "regular", None => "preserve" },
                validated.to_str().context("Temporary path is not UTF-8")?,
            ])?;
            transaction.prepare(&[
                "--config", SNAPSHOT_CONFIG,
                "--config", "merge-tools.zed-kk-resolution.program=\"/bin/sh\"",
                "--config", &format!("merge-tools.zed-kk-resolution.edit-args={arguments}"),
                "diffedit", "--from", "root()", "--to", &target, "--tool", "zed-kk-resolution", "--", &literal_fileset(&conflict.path)?,
            ]).await?;
            ensure!(smol::fs::read(&validated).await? == b"ok", "KK did not validate the conflict checkout");
            let prepared = transaction.snapshot();
            ensure!(!prepared.conflicted_paths(&target).await?.contains(&conflict.path), "The prepared result still contains a KK conflict");
            let changed = prepared.changed_files(&DiffScope::Comparison { base: current.full_commit_id, target: target.clone() }).await?;
            ensure!(changed.len() == 1 && changed.first().is_some_and(|file| file.path == conflict.path && file.renamed_from.is_none()),
                "Conflict resolution changed an unexpected path. Nothing was published.");
            ensure!(prepared.file_content_if_exists(&target, &conflict.path).await?.as_deref() == result,
                "The prepared resolution does not match the submitted result. Nothing was published.");
            anyhow::Ok(())
        }.await;
        smol::unblock(move || directory.close()).await?;
        prepared?;

        Ok(PreparedConflictResolution {
            transaction: transaction.into(),
            target,
        })
    }
}

fn validate_conflict_path(path: &str) -> Result<()> {
    crate::repository::validate_path(path)?;
    ensure!(
        !crate::repository::is_metadata_path(path),
        "Repository metadata cannot be modified"
    );
    Ok(())
}

// All paths and bytes are passed separately. A resolution must never evaluate file content as code.
const RESOLUTION_EDITOR: &str = r#"set -eu
target="$1/$2"
test -f "$target" && test ! -L "$target"
cmp -s "$target" "$3"
if [ "$5" = delete ]; then
    rm "$target"
else
    cat "$4" > "$target"
    if [ "$6" = executable ]; then chmod a+x "$target"; fi
    if [ "$6" = regular ]; then chmod a-x "$target"; fi
fi
printf ok > "$7"
"#;

pub fn parse_conflict_inputs(output: &str, path: &str) -> Result<Vec<ConflictInput>> {
    let mut remaining = output
        .trim_end()
        .strip_prefix(&format!("{path}: Ok(Conflicted(["))
        .and_then(|line| line.strip_suffix("]))"))
        .context("Cannot read regular-file conflict inputs with this KK version")?;
    let expression = Regex::new(
        r#"^(None|Some\(File \{ id: FileId\("([a-f0-9]{40}|[a-f0-9]{64})"\), executable: (true|false)(?:, copy_id: CopyId\("[a-f0-9]*"\))? \}\))"#,
    )?;
    let mut inputs = Vec::new();
    while !remaining.is_empty() {
        let captures = expression
            .captures(remaining)
            .context("Conflict resolution supports regular and deleted files")?;
        let position = inputs.len();
        inputs.push(ConflictInput {
            role: if position % 2 == 0 {
                ConflictRole::Side
            } else {
                ConflictRole::Base
            },
            index: position / 2,
            id: captures.get(2).map(|value| value.as_str().into()),
            executable: captures
                .get(3)
                .is_some_and(|value| value.as_str() == "true"),
            bytes: Vec::new(),
            source: None,
        });
        remaining = &remaining[captures.get(0).context("Missing conflict term")?.end()..];
        if !remaining.is_empty() {
            remaining = remaining
                .strip_prefix(", ")
                .filter(|remaining| !remaining.is_empty())
                .context("Unrecognized conflict input format")?;
        }
    }

    ensure!(
        inputs.len() >= 3 && inputs.len() % 2 == 1,
        "The selected file has no supported conflict"
    );
    let (mut sides, mut bases): (Vec<_>, Vec<_>) = inputs
        .into_iter()
        .partition(|input| input.role == ConflictRole::Side);
    // Native materialization cancels blob identities before rendering, independently of mode.
    for index in (0..bases.len()).rev() {
        if let Some(position) = sides.iter().position(|side| side.id == bases[index].id) {
            bases.remove(index);
            sides.remove(position);
        }
    }

    let mut result = Vec::with_capacity(sides.len() + bases.len());
    let mut bases = bases.into_iter();
    for (index, mut side) in sides.into_iter().enumerate() {
        side.index = index;
        result.push(side);
        if let Some(mut base) = bases.next() {
            base.index = index;
            result.push(base);
        }
    }

    Ok(result)
}

pub fn conflict_marker_length(inputs: &[ConflictInput]) -> usize {
    let longest = inputs
        .iter()
        .flat_map(|input| input.bytes.split(|byte| *byte == b'\n'))
        .filter_map(|line| {
            let first = line.first()?;
            if !b"<>+%|=\\-".contains(first) {
                return None;
            }

            let length = line.iter().take_while(|byte| *byte == first).count();
            line.get(length)
                .is_none_or(u8::is_ascii_whitespace)
                .then_some(length)
        })
        .max()
        .unwrap_or_default();
    7.max(longest + 4)
}

pub fn parse_conflict_hunks(text: &str, marker_length: usize) -> Result<Vec<ConflictHunk>> {
    ensure!(marker_length >= 7, "Invalid conflict marker length");
    if marker_length > text.len() {
        return Ok(Vec::new());
    }

    let start_marker = format!("{} conflict ", "<".repeat(marker_length));
    let end_marker = format!("{} conflict ", ">".repeat(marker_length));
    let side_marker = format!("{} ", "+".repeat(marker_length));
    let base_marker = format!("{} ", "-".repeat(marker_length));
    let mut hunks = Vec::new();
    let mut active: Option<ConflictHunk> = None;
    let mut offset = 0;
    let mut counts = [0, 0];
    let mut line_ending = "\n";
    for line in text.split_inclusive('\n') {
        let plain = line
            .strip_suffix('\n')
            .map(|line| line.strip_suffix('\r').unwrap_or(line))
            .unwrap_or(line);
        let start = plain.starts_with(&start_marker);
        let end = plain.starts_with(&end_marker);
        let section = plain
            .strip_prefix(&side_marker)
            .map(|label| (ConflictRole::Side, label, 0))
            .or_else(|| {
                plain
                    .strip_prefix(&base_marker)
                    .map(|label| (ConflictRole::Base, label, 1))
            });
        ensure!(
            !(active.is_none() && (end || section.is_some()) || active.is_some() && start),
            "Incomplete KK snapshot conflict"
        );
        if start {
            active = Some(ConflictHunk {
                range: offset..0,
                sections: Vec::new(),
            });
            counts = [0, 0];
            line_ending = if line.ends_with("\r\n") { "\r\n" } else { "\n" };
        } else if end {
            let mut hunk = active.take().context("Missing conflict block")?;
            ensure!(
                counts[0] >= 2 && counts[1] == counts[0] - 1,
                "Incomplete KK snapshot conflict"
            );
            if !line.ends_with('\n') {
                for section in &mut hunk.sections {
                    if let Some(text) = section.text.strip_suffix(line_ending) {
                        section.text.truncate(text.len());
                    }
                }
            }

            hunk.range.end = offset + line.len();
            hunks.push(hunk);
        } else if let Some(hunk) = &mut active {
            if let Some((role, label, count)) = section {
                hunk.sections.push(ConflictSection {
                    role,
                    index: counts[count],
                    label: label.into(),
                    text: String::new(),
                });
                counts[count] += 1;
            } else {
                hunk.sections
                    .last_mut()
                    .context("Unrecognized KK snapshot conflict")?
                    .text
                    .push_str(line);
            }
        }

        offset += line.len();
    }

    ensure!(active.is_none(), "Incomplete KK snapshot conflict");
    Ok(hunks)
}
