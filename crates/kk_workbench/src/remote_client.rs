use crate::{
    Diagnostics, Discovery, DiscoveryError, OperationReceipt, Publication, Repository, SavedFile,
    Snapshot, SparseCheckout, UncertainPublication,
    remote::{Failure, Mutation, Read, Request, ResourceId, Response, SnapshotInfo},
};
use anyhow::{Context as _, Result, bail, ensure};
use futures::{
    FutureExt as _,
    channel::oneshot,
    future::{BoxFuture, Either, Shared, select},
};
use serde::de::DeserializeOwned;
use std::{
    collections::HashMap,
    fmt,
    path::Path,
    sync::{Arc, OnceLock},
    time::Duration,
};

pub trait Transport: Send + Sync + 'static {
    // Sending must precede later editor saves; response consumption runs on the supplied executor.
    fn request(&self, request: Request) -> BoxFuture<'static, Result<Response>>;
    fn spawn(&self, future: BoxFuture<'static, ()>);
    fn timer(&self, duration: Duration) -> BoxFuture<'static, ()>;
}

#[derive(Clone)]
pub struct Client(Arc<dyn Transport>);

impl fmt::Debug for Client {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RemoteClient")
            .finish_non_exhaustive()
    }
}

impl Client {
    pub fn new(transport: Arc<dyn Transport>) -> Self {
        Self(transport)
    }

    pub(crate) fn same_connection(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    pub fn request(&self, request: Request) -> BoxFuture<'static, Result<Response>> {
        self.drive(self.0.request(request))
    }

    pub async fn configure_diagnostics(&self, categories: Vec<String>) -> Result<()> {
        ensure!(
            matches!(
                self.request(Request::ConfigureDiagnostics { categories })
                    .await?,
                Response::Done
            ),
            "Expected remote diagnostics configuration"
        );
        Ok(())
    }

    pub async fn export_draft(&self, path: std::path::PathBuf, text: String) -> Result<()> {
        ensure!(
            matches!(
                self.request(Request::ExportDraft { path, text }).await?,
                Response::Done
            ),
            "Expected remote draft export"
        );
        Ok(())
    }

    pub async fn diagnostics(
        &self,
        cursor: Option<crate::remote::DiagnosticCursor>,
        wait: bool,
    ) -> Result<crate::remote::DiagnosticOutput> {
        let Response::Data(bytes) = self.request(Request::Diagnostics { cursor, wait }).await?
        else {
            bail!("Expected remote diagnostics");
        };
        crate::remote::decode(&bytes)
    }

    fn drive<T: Send + 'static>(
        &self,
        work: BoxFuture<'static, Result<T>>,
    ) -> BoxFuture<'static, Result<T>> {
        let (sender, receiver) = oneshot::channel();
        self.0.spawn(
            async move {
                if let Err(Err(error)) = sender.send(work.await) {
                    log::error!("Released KK remote request failed: {error:#}");
                }
            }
            .boxed(),
        );
        async move {
            receiver
                .await
                .context("KK remote response worker was lost")?
        }
        .boxed()
    }

    pub async fn open(
        &self,
        directory: &Path,
        executable: &Path,
        diagnostics: Diagnostics,
    ) -> Result<Repository> {
        let response = self.request(Request::Open {
            directory: directory.to_owned(),
            executable: executable.to_owned(),
        });
        let client = self.clone();
        let executable = executable.to_owned();
        self.drive(
            async move {
                let Response::Repository(info) = response.await? else {
                    bail!("Expected remote KK repository")
                };
                Ok(Repository::from_remote(
                    client,
                    info,
                    &executable,
                    diagnostics,
                ))
            }
            .boxed(),
        )
        .await
    }

    pub async fn discover(
        &self,
        directory: &Path,
        executable: &Path,
        diagnostics: Diagnostics,
    ) -> Result<Discovery> {
        let response = self.request(Request::Discover {
            directory: directory.to_owned(),
            executable: executable.to_owned(),
        });
        let client = self.clone();
        let executable = executable.to_owned();
        self.drive(
            async move {
                let Response::Discovery {
                    repositories,
                    errors,
                } = response.await?
                else {
                    bail!("Expected remote KK discovery")
                };
                Ok(Discovery {
                    repositories: repositories
                        .into_iter()
                        .map(|info| {
                            Repository::from_remote(
                                client.clone(),
                                info,
                                &executable,
                                diagnostics.clone(),
                            )
                        })
                        .collect(),
                    errors: errors
                        .into_iter()
                        .map(|(path, message)| DiscoveryError { path, message })
                        .collect(),
                })
            }
            .boxed(),
        )
        .await
    }
}

#[derive(Debug)]
pub(crate) struct RemoteRepository {
    pub client: Client,
    pub id: ResourceId,
}

impl Drop for RemoteRepository {
    fn drop(&mut self) {
        drop(self.client.request(Request::ReleaseRepository {
            repository: self.id,
        }));
    }
}

impl RemoteRepository {
    pub fn detach(&self) {
        drop(self.client.request(Request::DetachRepository {
            repository: self.id,
        }));
    }

    pub fn info(&self, snapshot: &Snapshot) -> SnapshotInfo {
        SnapshotInfo {
            repository: self.id,
            operation: snapshot.operation().into(),
            performance_context: snapshot.repository().performance_context().cloned(),
        }
    }

    pub fn snapshot(&self, repository: &Repository, info: SnapshotInfo) -> Result<Snapshot> {
        ensure!(
            info.repository == self.id,
            "Remote snapshot belongs to another repository"
        );
        Snapshot::from_operation(repository.clone(), info.operation)
    }

    pub async fn snapshot_response(
        &self,
        repository: &Repository,
        request: Request,
    ) -> Result<Snapshot> {
        let Response::Snapshot(info) = self.client.request(request).await? else {
            bail!("Expected remote KK snapshot")
        };
        self.snapshot(repository, info)
    }

    pub async fn data<T: DeserializeOwned>(&self, request: Request) -> Result<T> {
        let Response::Data(bytes) = self.client.request(request).await? else {
            bail!("Expected remote KK data")
        };
        crate::remote::decode(&bytes)
    }

    pub async fn read<T: DeserializeOwned>(
        &self,
        snapshot: &Snapshot,
        operation: Read,
    ) -> Result<T> {
        self.data(Request::Read {
            snapshot: self.info(snapshot),
            operation,
        })
        .await
    }

    pub async fn prepare(
        &self,
        snapshot: &Snapshot,
        mutation: Mutation,
    ) -> Result<Option<RemotePrepared>> {
        snapshot.repository().check_lifetime()?;
        let id = uuid::Uuid::new_v4().as_u128();
        let resource = Resource {
            client: self.client.clone(),
            release: Request::ReleasePrepared { prepared: id },
        };
        let response = self
            .client
            .request(Request::Prepare {
                id,
                snapshot: self.info(snapshot),
                mutation,
            })
            .await?;
        snapshot.repository().check_lifetime()?;
        let Response::Prepared {
            id: returned,
            snapshot: prepared,
            receipt,
            destination,
            range_start,
        } = response
        else {
            ensure!(
                matches!(response, Response::Done),
                "Expected remote KK preparation"
            );
            return Ok(None);
        };
        ensure!(
            returned == id && receipt.repository == self.id,
            "Remote preparation belongs to another request"
        );
        Ok(Some(RemotePrepared {
            id,
            snapshot: self.snapshot(snapshot.repository(), prepared)?,
            receipt: OperationReceipt::decode(snapshot.repository().clone(), &receipt.data)?,
            destination,
            range_start,
            _resource: resource,
        }))
    }

    pub async fn sparse(
        &self,
        repository: &Repository,
        paths: &[String],
        is_current: impl Fn() -> bool + Send + Sync,
    ) -> Result<SparseCheckout> {
        if !is_current() {
            return Ok(SparseCheckout::Cancelled);
        }

        repository.check_lifetime()?;
        let id = uuid::Uuid::new_v4().as_u128();
        let _resource = Resource {
            client: self.client.clone(),
            release: Request::CancelSparseCheckout { id },
        };
        let mut response = self.client.request(Request::ExpandSparseCheckout {
            id,
            repository: self.id,
            paths: paths.to_vec(),
        });
        loop {
            match select(response, self.client.0.timer(Duration::from_millis(10))).await {
                Either::Left((result, _)) => {
                    return match result? {
                        Response::Snapshot(info) => {
                            Ok(SparseCheckout::Ready(self.snapshot(repository, info)?))
                        }
                        Response::SparsePendingSavedInputs => {
                            Ok(SparseCheckout::PendingSavedInputs)
                        }
                        Response::SparseCancelled => Ok(SparseCheckout::Cancelled),
                        _ => bail!("Expected remote sparse checkout result"),
                    };
                }
                Either::Right(((), pending)) => response = pending,
            }

            if !is_current() || repository.check_lifetime().is_err() {
                return Ok(SparseCheckout::Cancelled);
            }
        }
    }
}

struct Resource {
    client: Client,
    release: Request,
}

impl Drop for Resource {
    fn drop(&mut self) {
        drop(self.client.request(self.release.clone()));
    }
}

pub(crate) struct RemotePrepared {
    id: ResourceId,
    pub snapshot: Snapshot,
    pub receipt: OperationReceipt,
    pub destination: Option<crate::Revision>,
    pub range_start: Option<crate::Revision>,
    _resource: Resource,
}

impl RemotePrepared {
    pub async fn publish(self) -> Result<Publication> {
        let repository = self.snapshot.repository();
        repository.check_lifetime()?;
        let remote = repository.remote().context("Expected remote repository")?;
        let result = remote
            .client
            .request(Request::Publish { prepared: self.id })
            .await;
        let result = match result {
            Err(error)
                if error
                    .downcast_ref::<Failure>()
                    .is_some_and(|failure| failure.receipt.is_none()) =>
            {
                return Err(error);
            }
            Err(error) => Err(error),
            Ok(Response::Published(published)) => (|| {
                ensure!(
                    published.receipt.repository == remote.id,
                    "Publication belongs to another repository"
                );
                Ok(Publication {
                    receipt: OperationReceipt::decode(repository.clone(), &published.receipt.data)?,
                    snapshot: remote.snapshot(repository, published.snapshot)?,
                    synchronization_error: published.synchronization_error,
                })
            })(),
            Ok(_) => Err(anyhow::anyhow!("Expected remote KK publication")),
        };
        result.map_err(|reason| {
            UncertainPublication {
                receipt: self.receipt.clone(),
                reason,
            }
            .into()
        })
    }
}

struct Captured {
    version: u64,
    files: HashMap<String, SavedFile>,
}

type CaptureResult = std::result::Result<Captured, Arc<str>>;

pub(crate) struct RemoteSaved {
    pub id: ResourceId,
    repository: Repository,
    captured: Arc<OnceLock<CaptureResult>>,
    completed: Shared<BoxFuture<'static, ()>>,
    _resource: Resource,
}

impl RemoteSaved {
    pub fn capture(repository: &Repository, paths: Vec<String>) -> Result<Self> {
        repository.check_lifetime()?;
        let remote = repository.remote().context("Expected remote repository")?;
        let id = uuid::Uuid::new_v4().as_u128();
        let resource = Resource {
            client: remote.client.clone(),
            release: Request::ReleaseSaved { capture: id },
        };
        let response = remote.client.request(Request::CaptureSaved {
            id,
            repository: remote.id,
            paths,
        });
        let captured = Arc::new(OnceLock::new());
        let (sender, receiver) = oneshot::channel();
        remote.client.0.spawn({
            let captured = captured.clone();
            async move {
                let result = async {
                    let Response::Saved {
                        id: returned,
                        version,
                        files,
                    } = response.await?
                    else {
                        bail!("Expected saved remote files")
                    };
                    ensure!(returned == id, "Saved capture belongs to another request");
                    Ok(Captured { version, files })
                }
                .await
                .map_err(|error: anyhow::Error| Arc::<str>::from(format!("{error:#}")));
                if captured.set(result).is_err() {
                    log::error!("Remote saved capture was already recorded");
                }

                if sender.send(()).is_err() {
                    log::debug!("Remote saved capture waiter was released");
                }
            }
            .boxed()
        });
        Ok(Self {
            id,
            repository: repository.clone(),
            captured,
            completed: receiver.map(|_| ()).boxed().shared(),
            _resource: resource,
        })
    }

    fn captured(&self) -> Result<&Captured> {
        self.captured
            .get()
            .context("Remote saved capture is still pending")?
            .as_ref()
            .map_err(|error| anyhow::anyhow!(error.to_string()))
    }

    pub async fn resolve(&self, repository: &Repository) -> Result<ResourceId> {
        self.repository.check_lifetime()?;
        repository.check_lifetime()?;
        ensure!(
            self.repository.same_location(repository),
            "Saved inputs belong to another remote repository"
        );
        self.completed.clone().await;
        self.captured()?;
        Ok(self.id)
    }

    pub fn version(&self) -> Option<u64> {
        self.captured
            .get()?
            .as_ref()
            .ok()
            .map(|captured| captured.version)
    }

    pub fn files(&self) -> Result<&HashMap<String, SavedFile>> {
        Ok(&self.captured()?.files)
    }
    pub fn root(&self) -> &Path {
        self.repository.root()
    }
    pub fn verify(&self) -> Result<()> {
        self.captured().map(|_| ())
    }

    pub async fn finish(&self) -> Result<Option<Snapshot>> {
        self.completed.clone().await;
        self.captured()?;
        let remote = self
            .repository
            .remote()
            .context("Expected remote repository")?;
        match remote
            .client
            .request(Request::FinishSaved { capture: self.id })
            .await?
        {
            Response::Done => Ok(None),
            Response::Snapshot(info) => remote.snapshot(&self.repository, info).map(Some),
            _ => bail!("Expected remote saved-file finalization"),
        }
    }
}
