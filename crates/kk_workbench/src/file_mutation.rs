use crate::{
    ChangeKind, ChangedFile, DiffScope, Publication, Revision, Snapshot, transaction::Transaction,
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
pub enum FileMoveDirection {
    Parent,
    Child,
}

pub struct PreparedFileMutation {
    transaction: crate::transaction::PreparedTransaction,
    targets: Vec<String>,
}

impl PreparedFileMutation {
    pub fn snapshot(&self) -> &Snapshot {
        self.transaction.snapshot()
    }

    pub(crate) async fn receipt(&self) -> Result<crate::OperationReceipt> {
        self.transaction.file_change_receipt(&self.targets).await
    }

    pub async fn publish(self) -> Result<Publication> {
        self.transaction.publish_file_change(&self.targets).await
    }
}

impl Snapshot {
    pub async fn verify_file_version(
        &self,
        previous: &str,
        current: &str,
        file: &ChangedFile,
    ) -> Result<()> {
        if let Some(remote) = self.repository().remote() {
            return remote
                .read(
                    self,
                    crate::remote::Read::VerifyFileVersion {
                        previous: previous.into(),
                        current: current.into(),
                        file: file.clone(),
                    },
                )
                .await;
        }

        mutation_patterns(file)?;
        let paths = std::iter::once(file.path.as_str()).chain(
            (file.change_type == ChangeKind::Renamed)
                .then_some(file.renamed_from.as_deref())
                .flatten(),
        );
        for path in paths {
            ensure!(
                self.diff(
                    &DiffScope::Comparison {
                        base: previous.into(),
                        target: current.into()
                    },
                    Some(path)
                )
                .await?
                .is_empty(),
                "The selected file changed while this action was pending. Your queued action was not applied; select the updated file to retry."
            );
        }

        Ok(())
    }

    pub async fn prepare_file_move(
        &self,
        source: &Revision,
        direction: FileMoveDirection,
        file: &ChangedFile,
    ) -> Result<PreparedFileMutation> {
        if let Some(remote) = self.repository().remote() {
            let prepared = remote
                .prepare(
                    self,
                    crate::remote::Mutation::Move {
                        source: source.clone(),
                        direction,
                        file: file.clone(),
                    },
                )
                .await?
                .ok_or_else(|| anyhow::anyhow!("Missing remote file move"))?;
            return Ok(PreparedFileMutation {
                transaction: crate::transaction::PreparedTransaction::Remote(prepared),
                targets: Vec::new(),
            });
        }

        let patterns = mutation_patterns(file)?;
        let mut transaction = Transaction::begin(self).await?;
        let current_source = self.exact_revision(&source.revset()).await?;
        ensure!(
            current_source.full_commit_id == source.full_commit_id,
            "The source changed. Refresh before moving the file."
        );
        ensure!(
            self.changed_files(&DiffScope::Change(source.full_commit_id.clone()))
                .await?
                .contains(file),
            "The selected file or rename relationship changed"
        );
        let relation = match direction {
            FileMoveDirection::Parent => "parents",
            FileMoveDirection::Child => "children",
        };
        let destination = self
            .exact_revision(&format!("{relation}({})", source.full_commit_id))
            .await?;
        let direct_destination = format!(
            "exactly({relation}({}), 1) & {}",
            source.revset(),
            destination.revset()
        );
        let working_copy = self.exact_revision("@").await?;
        let preserved = if direction == FileMoveDirection::Child {
            self.revisions(
                Some(&format!("descendants({})", destination.full_commit_id)),
                usize::MAX,
            )
            .await?
        } else {
            Vec::new()
        };

        let source_revset = source.revset();
        let mut arguments = vec![
            "squash",
            "--from",
            &source_revset,
            "--into",
            &direct_destination,
            "--use-destination-message",
            "--",
        ];
        arguments.extend(patterns.iter().map(String::as_str));
        transaction.prepare(&arguments).await?;

        for before in preserved {
            let revision = before.revset();
            if !trees_equal(transaction.snapshot(), &before.full_commit_id, &revision).await? {
                let mut arguments = vec![
                    "restore",
                    "--from",
                    &before.full_commit_id,
                    "--into",
                    &revision,
                    "--restore-descendants",
                    "--",
                ];
                arguments.extend(patterns.iter().map(String::as_str));
                transaction.prepare(&arguments).await?;
                ensure!(
                    trees_equal(transaction.snapshot(), &before.full_commit_id, &revision).await?,
                    "The move changed a protected descendant. Nothing was published."
                );
            }
        }

        ensure!(
            trees_equal(transaction.snapshot(), &working_copy.full_commit_id, "@").await?,
            "The move changed working-copy content. Nothing was published."
        );

        Ok(PreparedFileMutation {
            transaction: transaction.into(),
            targets: vec![source.revset(), destination.revset()],
        })
    }

    pub async fn prepare_file_revert(
        &self,
        scope: &DiffScope,
        file: &ChangedFile,
    ) -> Result<PreparedFileMutation> {
        if let Some(remote) = self.repository().remote() {
            let prepared = remote
                .prepare(
                    self,
                    crate::remote::Mutation::Revert {
                        scope: scope.clone(),
                        file: file.clone(),
                    },
                )
                .await?
                .ok_or_else(|| anyhow::anyhow!("Missing remote file revert"))?;
            return Ok(PreparedFileMutation {
                transaction: crate::transaction::PreparedTransaction::Remote(prepared),
                targets: Vec::new(),
            });
        }

        let patterns = mutation_patterns(file)?;
        let mut transaction = Transaction::begin(self).await?;
        let working_copy = self.exact_revision("@").await?;
        let from = match scope {
            DiffScope::Change(revision) => {
                let revision = self.exact_revision(revision).await?;
                ensure!(
                    revision.full_commit_id == working_copy.full_commit_id,
                    "Revert is only available for the working copy or a comparison with it"
                );
                self.exact_revision(&format!("parents({})", revision.full_commit_id))
                    .await?
            }
            DiffScope::Comparison { base, target } => {
                ensure!(
                    self.exact_revision(target).await?.full_commit_id
                        == working_copy.full_commit_id,
                    "Revert requires a comparison with the working copy"
                );
                self.exact_revision(base).await?
            }
        };
        ensure!(
            self.changed_files(scope).await?.contains(file),
            "The selected file or rename relationship changed"
        );
        let working_revset = working_copy.revset();
        let mut arguments = vec![
            "restore",
            "--from",
            &from.full_commit_id,
            "--into",
            &working_revset,
            "--",
        ];
        arguments.extend(patterns.iter().map(String::as_str));
        transaction.prepare(&arguments).await?;
        let paths: Vec<_> = std::iter::once(file.path.as_str())
            .chain(
                (file.change_type == ChangeKind::Renamed)
                    .then_some(file.renamed_from.as_deref())
                    .flatten(),
            )
            .collect();
        let actual = transaction
            .snapshot()
            .changed_files(&DiffScope::Comparison {
                base: working_copy.full_commit_id,
                target: "@".into(),
            })
            .await?;
        ensure!(
            actual.iter().all(|file| paths.contains(&file.path.as_str())
                && file
                    .renamed_from
                    .as_deref()
                    .is_none_or(|path| paths.contains(&path))),
            "Revert changed an unselected file. Nothing was published."
        );
        for path in paths {
            let diff = transaction
                .snapshot()
                .diff(
                    &DiffScope::Comparison {
                        base: from.full_commit_id.clone(),
                        target: "@".into(),
                    },
                    Some(path),
                )
                .await?;
            ensure!(
                diff.is_empty(),
                "Revert did not reproduce the selected version. Nothing was published."
            );
        }

        Ok(PreparedFileMutation {
            transaction: transaction.into(),
            targets: Vec::new(),
        })
    }
}

fn mutation_patterns(file: &ChangedFile) -> Result<Vec<String>> {
    let mut paths = vec![file.path.as_str()];
    if file.change_type == ChangeKind::Renamed {
        paths.push(
            file.renamed_from
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("A renamed file requires its original path"))?,
        );
    }

    paths
        .into_iter()
        .map(|path| {
            ensure!(
                !crate::repository::is_metadata_path(path),
                "Repository metadata cannot be modified"
            );
            crate::repository::literal_fileset(path)
        })
        .collect()
}

async fn trees_equal(snapshot: &Snapshot, base: &str, target: &str) -> Result<bool> {
    Ok(snapshot
        .changed_files(&DiffScope::Comparison {
            base: base.into(),
            target: target.into(),
        })
        .await?
        .is_empty())
}
