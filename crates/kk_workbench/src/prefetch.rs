use crate::{FileInformation, Snapshot, remote::Read};
use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

pub const PREFETCH_FILE_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FileProbe {
    pub information: FileInformation,
    pub contents: Option<Vec<u8>>,
}

impl FileProbe {
    pub fn is_text(&self) -> bool {
        self.contents.is_some() && self.information.pointer.is_none()
    }
}

impl Snapshot {
    pub async fn file_probe(&self, revision: &str, path: &str) -> Result<FileProbe> {
        self.repository().check_lifetime()?;
        let probe = self
            .read_cached(
                Read::FileProbe {
                    revision: revision.into(),
                    path: path.into(),
                },
                Box::pin(self.file_probe_uncached(revision, path)),
            )
            .await?;
        self.repository().check_lifetime()?;
        Ok(probe)
    }

    pub async fn prefetch_file(&self, revision: &str, path: &str) -> Result<FileProbe> {
        let probe = self.file_probe(revision, path).await?;
        if probe.contents.is_some() || probe.information.exists == Some(false) {
            self.read_cached(
                Read::FileContentIfExists {
                    revision: revision.into(),
                    path: path.into(),
                },
                async { Ok(probe.contents.clone()) },
            )
            .await?;
        }

        if let Some(contents) = &probe.contents {
            self.read_cached(
                Read::FileContent {
                    revision: revision.into(),
                    path: path.into(),
                },
                async { Ok(contents.clone()) },
            )
            .await?;
        }

        if probe.information.hash.is_some()
            || probe.information.file_type.as_deref() != Some("file")
        {
            self.read_cached(
                Read::FileInformation {
                    revision: revision.into(),
                    path: path.into(),
                },
                async { Ok(probe.information.clone()) },
            )
            .await?;
        }

        Ok(probe)
    }

    async fn file_probe_uncached(&self, revision: &str, path: &str) -> Result<FileProbe> {
        if let Some(remote) = self.repository().remote() {
            return remote
                .read(
                    self,
                    Read::FileProbe {
                        revision: revision.into(),
                        path: path.into(),
                    },
                )
                .await;
        }

        let mut information = self.file_metadata(revision, path).await?;
        let mut contents = None;
        if information.file_type.as_deref() == Some("file") {
            let object = information
                .object_id
                .as_deref()
                .context("Missing file object ID")?;
            let size = self.repository().file_object_size(object).await?;
            information.size = Some(size);
            if size <= PREFETCH_FILE_BYTES as u64 {
                let bytes = self.file_content_uncached(revision, path).await?;
                information.contents(&bytes);
                if !bytes.contains(&0) && std::str::from_utf8(&bytes).is_ok() {
                    contents = Some(bytes);
                }
            }
        }

        Ok(FileProbe {
            information,
            contents,
        })
    }
}
