use crate::Snapshot;
use anyhow::{Context as _, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use smol::lock::{Mutex, MutexGuardArc};
use std::{
    collections::HashMap,
    fmt,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, Weak},
};

type WriteLocks = std::sync::Mutex<HashMap<PathBuf, Weak<Mutex<()>>>>;
static WRITE_LOCKS: LazyLock<WriteLocks> = LazyLock::new(WriteLocks::default);

pub(crate) async fn write_lock(root: &Path) -> Result<MutexGuardArc<()>> {
    let lock = {
        let mut locks = WRITE_LOCKS
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository write queue is unavailable"))?;
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(root).and_then(Weak::upgrade) {
            lock
        } else {
            let lock = Arc::new(Mutex::new(()));
            locks.insert(root.to_owned(), Arc::downgrade(&lock));
            lock
        }
    };
    Ok(lock.lock_arc().await)
}

#[derive(Clone, Debug)]
pub struct OperationReceipt {
    pub previous: Snapshot,
    pub prepared: Snapshot,
    signature: Option<PublicationSignature>,
    deferred_working_copy: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct PublicationSignature {
    revset: String,
    before: Vec<String>,
    after: Vec<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum PublicationStatus {
    Applied,
    Rejected,
    Unknown,
}

#[derive(Debug)]
pub struct Publication {
    pub receipt: OperationReceipt,
    pub snapshot: Snapshot,
    pub synchronization_error: Option<String>,
}

#[derive(Debug)]
pub struct UncertainPublication {
    pub receipt: OperationReceipt,
    pub reason: anyhow::Error,
}

impl fmt::Display for UncertainPublication {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "Unable to confirm whether the operation finished. Refresh KK Workbench before retrying: {}",
            self.reason
        )
    }
}

impl std::error::Error for UncertainPublication {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.reason.as_ref())
    }
}

impl OperationReceipt {
    pub fn with_repository(&self, repository: &crate::Repository) -> Result<Self> {
        let previous_repository = self.previous.repository();
        previous_repository.check_lifetime()?;
        repository.check_lifetime()?;
        ensure!(
            previous_repository.same_location(repository),
            "Recovery receipt belongs to another repository"
        );
        let repository = match previous_repository.lifetime() {
            Some(lifetime) => repository.with_lifetime(lifetime.clone()),
            None => repository.clone(),
        };
        Ok(Self {
            previous: Snapshot::from_operation(
                repository.clone(),
                self.previous.operation().to_owned(),
            )?,
            prepared: Snapshot::from_operation(repository, self.prepared.operation().to_owned())?,
            signature: self.signature.clone(),
            deferred_working_copy: self.deferred_working_copy,
        })
    }

    pub(crate) fn encode(&self) -> Result<Vec<u8>> {
        crate::remote::encode(&(
            self.previous.operation(),
            self.prepared.operation(),
            &self.signature,
            self.deferred_working_copy,
        ))
    }

    pub(crate) fn decode(repository: crate::Repository, bytes: &[u8]) -> Result<Self> {
        let (previous, prepared, signature, deferred_working_copy): (
            String,
            String,
            Option<PublicationSignature>,
            bool,
        ) = crate::remote::decode(bytes)?;
        Ok(Self {
            previous: Snapshot::from_operation(repository.clone(), previous)?,
            prepared: Snapshot::from_operation(repository, prepared)?,
            signature,
            deferred_working_copy,
        })
    }

    pub async fn synchronize(&self) -> Result<Snapshot> {
        let repository = self.previous.repository();
        repository.check_lifetime()?;
        if let Some(remote) = repository.remote() {
            return remote
                .snapshot_response(
                    repository,
                    crate::remote::Request::Synchronize {
                        receipt: crate::remote::Receipt {
                            repository: remote.id,
                            data: self.encode()?,
                        },
                    },
                )
                .await;
        }

        let _write_guard = write_lock(repository.root()).await?;
        repository.check_lifetime()?;
        ensure!(
            self.status().await? == PublicationStatus::Applied,
            "The operation has not been confirmed in the repository history"
        );
        if self.deferred_working_copy && crate::saved::is_deferred(repository.root())? {
            repository.snapshot_unlocked(false).await
        } else {
            repository.command(&["workspace", "update-stale"]).await?;
            repository.snapshot_unlocked(true).await
        }
    }

    pub async fn status(&self) -> Result<PublicationStatus> {
        if let Some(remote) = self.previous.repository().remote() {
            return remote
                .data(crate::remote::Request::PublicationStatus {
                    receipt: crate::remote::Receipt {
                        repository: remote.id,
                        data: self.encode()?,
                    },
                })
                .await;
        }

        if let Some(signature) = &self.signature {
            let current = self.previous.repository().snapshot_unlocked(false).await?;
            let actual = revision_signature(&current, &signature.revset).await?;
            return Ok(if actual == signature.after {
                PublicationStatus::Applied
            } else if actual == signature.before {
                PublicationStatus::Rejected
            } else {
                PublicationStatus::Unknown
            });
        }

        let output = self
            .previous
            .repository()
            .command(&[
                "--ignore-working-copy",
                "op",
                "log",
                "--limit",
                "64",
                "--no-graph",
                "-T",
                "id ++ \"\\n\"",
            ])
            .await?;
        let operations = std::str::from_utf8(&output.stdout)?;
        if operations
            .lines()
            .any(|operation| operation.starts_with(self.prepared.operation()))
        {
            return Ok(PublicationStatus::Applied);
        }

        if operations
            .lines()
            .any(|operation| operation == self.previous.operation())
        {
            return Ok(PublicationStatus::Rejected);
        }

        Ok(PublicationStatus::Unknown)
    }
}

pub(crate) enum PreparedTransaction {
    Local(Transaction),
    Remote(crate::remote_client::RemotePrepared),
}

impl From<Transaction> for PreparedTransaction {
    fn from(transaction: Transaction) -> Self {
        Self::Local(transaction)
    }
}

impl PreparedTransaction {
    pub fn snapshot(&self) -> &Snapshot {
        match self {
            Self::Local(transaction) => transaction.snapshot(),
            Self::Remote(prepared) => &prepared.snapshot,
        }
    }

    pub async fn file_change_receipt(&self, targets: &[String]) -> Result<OperationReceipt> {
        match self {
            Self::Local(transaction) => transaction.file_change_receipt(targets).await,
            Self::Remote(prepared) => Ok(prepared.receipt.clone()),
        }
    }

    pub async fn publish_file_change(self, targets: &[String]) -> Result<Publication> {
        match self {
            Self::Local(transaction) => transaction.publish_file_change(targets).await,
            Self::Remote(prepared) => prepared.publish().await,
        }
    }
}

pub(crate) struct Transaction {
    original: Snapshot,
    current: Snapshot,
    _write_guard: MutexGuardArc<()>,
    saved_inputs: Vec<crate::SavedSnapshot>,
    working_copy_before_inputs: Option<String>,
    yield_to_openings: bool,
}

impl Transaction {
    pub(crate) async fn begin(snapshot: &Snapshot) -> Result<Self> {
        Self::begin_with_saved(snapshot, Vec::new()).await
    }

    pub(crate) async fn begin_with_saved(
        snapshot: &Snapshot,
        saved_inputs: Vec<crate::SavedSnapshot>,
    ) -> Result<Self> {
        ensure!(
            !snapshot.repository().is_remote(),
            "Remote transactions must be prepared on the server"
        );
        snapshot.repository().check_lifetime()?;
        let write_guard = write_lock(snapshot.repository().root()).await?;
        snapshot.repository().check_lifetime()?;
        for saved in &saved_inputs {
            ensure!(
                saved.root() == snapshot.repository().root(),
                "Saved inputs belong to another repository"
            );
            saved.verify().await?;
        }

        let current = snapshot
            .repository()
            .snapshot_unlocked(saved_inputs.is_empty())
            .await?;
        ensure!(
            current.operation() == snapshot.operation(),
            "The repository changed since this view was loaded. Refresh before changing its history."
        );
        Ok(Self {
            original: snapshot.clone(),
            current,
            _write_guard: write_guard,
            saved_inputs,
            working_copy_before_inputs: None,
            yield_to_openings: false,
        })
    }

    pub(crate) fn snapshot(&self) -> &Snapshot {
        &self.current
    }

    pub(crate) fn prioritize_diff_opening(&mut self) {
        self.yield_to_openings = true;
    }

    pub(crate) async fn prepare_inputs(
        &mut self,
        files: &HashMap<String, crate::SavedFile>,
    ) -> Result<()> {
        let before = self.current.exact_revision("@").await?.full_commit_id;
        if self.prepare_saved_files(files).await? {
            self.working_copy_before_inputs = Some(before);
        }

        Ok(())
    }

    pub(crate) async fn restore_inputs(&mut self) -> Result<()> {
        if let Some(before) = self.working_copy_before_inputs.take() {
            self.prepare(&[
                "restore",
                "--from",
                &before,
                "--into",
                "@",
                "--restore-descendants",
            ])
            .await?;
            ensure!(
                self.current
                    .changed_files(&crate::DiffScope::Comparison {
                        base: before,
                        target: "@".into()
                    })
                    .await?
                    .is_empty(),
                "Classification did not preserve the recorded working-copy tree. Nothing was published."
            );
        }

        Ok(())
    }

    pub(crate) async fn prepare(&mut self, command_arguments: &[&str]) -> Result<()> {
        if self.yield_to_openings {
            self.original.repository().yield_to_diff_opening().await?;
        }

        let mut arguments = vec![
            "--at-operation",
            self.current.operation(),
            "--no-integrate-operation",
        ];
        arguments.extend_from_slice(command_arguments);
        let output = self.current.repository().command(&arguments).await?;
        let output = format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        if let Some((_, operation)) = output.split_once("--no-integrate-operation was requested:") {
            let operation = operation
                .split_whitespace()
                .next()
                .context("Missing private operation ID")?;
            ensure!(
                (12..=128).contains(&operation.len())
                    && operation.bytes().all(|byte| byte.is_ascii_hexdigit()),
                "KK returned an invalid private operation ID"
            );
            self.current = self.current.at_operation(operation.into());
        } else if !output.contains("Nothing changed.") {
            bail!(
                "KK did not return a private operation ID. Update the configured KK executable and retry."
            );
        }

        Ok(())
    }

    pub(crate) async fn publish(self) -> Result<Publication> {
        self.publish_command(None).await
    }

    pub(crate) fn receipt(&self) -> OperationReceipt {
        OperationReceipt {
            previous: self.original.clone(),
            prepared: self.current.clone(),
            signature: None,
            deferred_working_copy: !self.saved_inputs.is_empty(),
        }
    }

    pub(crate) async fn file_change_receipt(&self, targets: &[String]) -> Result<OperationReceipt> {
        let mut receipt = self.receipt();
        if self.current.operation() == self.original.operation() {
            return Ok(receipt);
        }

        let deferred = !self.saved_inputs.is_empty();
        let working_copy = self.original.exact_revision("@").await?.revset();
        // Later saves replace the working-copy commit without changing its parents.
        let revset = std::iter::once(if deferred { "parents(@)" } else { "@" }.to_owned())
            .chain(
                targets
                    .iter()
                    .filter(|target| !deferred || **target != working_copy)
                    .map(|target| format!("present({target})")),
            )
            .collect::<Vec<_>>()
            .join(" | ");
        receipt.signature = Some(PublicationSignature {
            before: revision_signature(&self.original, &revset).await?,
            after: revision_signature(&self.current, &revset).await?,
            revset,
        });
        Ok(receipt)
    }

    pub(crate) async fn publish_file_change(mut self, targets: &[String]) -> Result<Publication> {
        let Some(signature) = self.file_change_receipt(targets).await?.signature else {
            return self.publish().await;
        };
        let prepared = self.current.clone();
        let original = self.original.operation().to_owned();
        self.prepare(&["op", "restore", &original]).await?;
        let inverse = self.current.operation().to_owned();
        self.current = prepared;
        self.publish_command(Some((inverse, signature))).await
    }

    async fn publish_command(
        self,
        inverse: Option<(String, PublicationSignature)>,
    ) -> Result<Publication> {
        let repository = self.original.repository();
        if self.yield_to_openings {
            repository.yield_to_diff_opening().await?;
        }
        repository.check_lifetime()?;
        for saved in &self.saved_inputs {
            saved.verify().await?;
        }

        let deferred = !self.saved_inputs.is_empty();
        let current = repository.snapshot_unlocked(!deferred).await?;
        ensure!(
            current.operation() == self.original.operation(),
            "The repository or working files changed while this operation was being prepared. Nothing was published."
        );
        let mut receipt = self.receipt();
        receipt.signature = inverse.as_ref().map(|(_, signature)| signature.clone());
        let command = match &inverse {
            Some((operation, _)) if deferred => {
                vec!["--ignore-working-copy", "op", "revert", operation.as_str()]
            }
            Some((operation, _)) => vec!["op", "revert", operation.as_str()],
            None if deferred => vec![
                "--ignore-working-copy",
                "op",
                "integrate",
                self.current.operation(),
            ],
            None => vec!["op", "integrate", self.current.operation()],
        };
        repository.check_lifetime()?;
        if self.current.operation() != self.original.operation()
            && let Err(error) = repository.command(&command).await
        {
            let status = match receipt.status().await {
                Ok(PublicationStatus::Unknown) | Err(_) => receipt.status().await,
                status => status,
            };
            match status {
                Ok(PublicationStatus::Applied) => {}
                Ok(PublicationStatus::Rejected) => return Err(error),
                Ok(PublicationStatus::Unknown) | Err(_) => {
                    return Err(UncertainPublication {
                        receipt,
                        reason: error,
                    }
                    .into());
                }
            }
        }

        // Publication is already durable. A checkout error must not invite a second rewrite.
        let synchronization_error = if deferred {
            None
        } else {
            repository
                .command(&["workspace", "update-stale"])
                .await
                .err()
                .map(|error| error.to_string())
        };
        let snapshot = match repository.snapshot_unlocked(false).await {
            Ok(snapshot) => snapshot,
            Err(error) => {
                return Err(UncertainPublication {
                    receipt,
                    reason: error,
                }
                .into());
            }
        };
        if inverse.is_some() {
            receipt.prepared = snapshot.clone();
            receipt.signature = None;
        }

        Ok(Publication {
            receipt,
            snapshot,
            synchronization_error,
        })
    }
}

async fn revision_signature(snapshot: &Snapshot, revset: &str) -> Result<Vec<String>> {
    let mut commits = snapshot
        .revisions(Some(revset), usize::MAX)
        .await?
        .into_iter()
        .map(|revision| revision.full_commit_id)
        .collect::<Vec<_>>();
    commits.sort();
    Ok(commits)
}
