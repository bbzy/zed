use crate::{
    Repository, Snapshot, classification::validate_selection_path, repository::literal_fileset,
    transaction::Transaction,
};
use anyhow::{Context as _, Result, ensure};
use futures::{
    FutureExt as _,
    channel::oneshot,
    future::{BoxFuture, Shared},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, LazyLock, Mutex, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

static WAVES: LazyLock<Mutex<HashMap<PathBuf, Weak<SaveWave>>>> = LazyLock::new(Mutex::default);
static NEXT_VERSION: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct SavedFile {
    pub bytes: Option<Arc<[u8]>>,
    pub executable: bool,
}

impl SavedFile {
    fn read(root: &Path, path: &str) -> Result<Self> {
        validate_selection_path(path)?;
        let absolute = root.join(path);
        let metadata = match fs::symlink_metadata(&absolute) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self {
                    bytes: None,
                    executable: false,
                });
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("Could not capture saved file {}", absolute.display())
                });
            }
        };
        ensure!(
            metadata.is_file(),
            "Classification requires a regular file: {}",
            absolute.display()
        );
        let parent = fs::canonicalize(absolute.parent().context("File has no parent")?)?;
        let relative = parent
            .strip_prefix(root)
            .context("Selected file resolves outside the repository")?;
        for component in relative.components() {
            ensure!(
                !matches!(component.as_os_str().to_str(), Some(".kk" | ".jj" | ".git")),
                "Repository metadata cannot be classified"
            );
        }

        let bytes = fs::read(&absolute)?;
        let after = fs::symlink_metadata(&absolute)?;
        ensure!(
            after.is_file()
                && metadata.len() == after.len()
                && metadata.modified()? == after.modified()?,
            "The file changed while its saved version was being captured: {}",
            absolute.display()
        );
        #[cfg(unix)]
        let executable = {
            use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
            ensure!(
                metadata.permissions().mode() == after.permissions().mode()
                    && metadata.ino() == after.ino()
                    && metadata.dev() == after.dev(),
                "The file or its mode changed during capture"
            );
            metadata.permissions().mode() & 0o111 != 0
        };
        #[cfg(not(unix))]
        let executable = false;
        Ok(Self {
            bytes: Some(bytes.into()),
            executable,
        })
    }
}

struct SaveWave {
    state: Mutex<SaveWaveState>,
}

struct SaveWaveState {
    users: usize,
    version: u64,
    saved: HashMap<String, SavedFile>,
    failure: Option<String>,
    pending: HashMap<u64, Shared<BoxFuture<'static, ()>>>,
}

pub struct SavedFileWrite {
    id: u64,
    files: Vec<(PathBuf, String, Arc<SaveWave>)>,
    completed: bool,
    completion: Option<oneshot::Sender<()>>,
}

impl SavedFileWrite {
    pub fn begin(absolute: &Path) -> Result<Self> {
        let waves = WAVES
            .lock()
            .map_err(|_| anyhow::anyhow!("Saved-file captures are unavailable"))?
            .iter()
            .filter_map(|(root, wave)| wave.upgrade().map(|wave| (root.clone(), wave)))
            .collect::<Vec<_>>();
        let (completion, completed) = oneshot::channel();
        let pending = completed.map(|_| ()).boxed().shared();
        let mut write = Self {
            id: NEXT_VERSION.fetch_add(1, Ordering::Relaxed),
            files: Vec::new(),
            completed: false,
            completion: Some(completion),
        };
        if waves.is_empty() {
            return Ok(write);
        }

        let parent = absolute.parent().context("Saved file has no parent")?;
        let canonical = match fs::canonicalize(parent) {
            Ok(parent) => parent.join(absolute.file_name().context("Saved file has no name")?),
            Err(_) if waves.iter().all(|(root, _)| !absolute.starts_with(root)) => {
                return Ok(write);
            }
            Err(error) => return Err(error.into()),
        };
        for (root, wave) in waves {
            let Ok(path) = canonical.strip_prefix(&root) else {
                continue;
            };
            let path = path
                .to_str()
                .context("Saved file path is not UTF-8")?
                .to_owned();
            let mut state = wave
                .state
                .lock()
                .map_err(|_| anyhow::anyhow!("Saved-file capture is unavailable"))?;
            if state.users > 0 {
                state.pending.insert(write.id, pending.clone());
                drop(state);
                write.files.push((root, path, wave));
            }
        }

        Ok(write)
    }

    pub fn finish(mut self) -> Result<()> {
        for (root, path, _) in &self.files {
            record_saved_file(root, path)?;
        }

        self.completed = true;
        Ok(())
    }
}

impl Drop for SavedFileWrite {
    fn drop(&mut self) {
        for (_, _, wave) in &self.files {
            match wave.state.lock() {
                Ok(mut state) => {
                    if !self.completed {
                        state.failure = Some("An editor save failed or was cancelled".into());
                    }

                    state.pending.remove(&self.id);
                }
                Err(error) => log::error!("Could not finish saved-file write: {error}"),
            }
        }

        drop(self.completion.take());
    }
}

struct SavedCapture {
    repository: Repository,
    wave: Arc<SaveWave>,
    files: HashMap<String, SavedFile>,
    version: u64,
    released: AtomicBool,
}

impl SavedCapture {
    async fn release_after_saves(&self) -> Result<bool> {
        loop {
            let pending = {
                let mut state = self
                    .wave
                    .state
                    .lock()
                    .map_err(|_| anyhow::anyhow!("Saved-file capture is unavailable"))?;
                if self.released.load(Ordering::Acquire) {
                    return Ok(false);
                }

                if state.pending.is_empty() {
                    self.released.store(true, Ordering::Release);
                    state.users = state.users.saturating_sub(1);
                    return Ok(state.users == 0);
                }

                state.pending.values().cloned().collect::<Vec<_>>()
            };
            futures::future::join_all(pending).await;
        }
    }

    fn release(&self) -> Result<()> {
        if self.released.swap(true, Ordering::AcqRel) {
            return Ok(());
        }

        let mut state = self
            .wave
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("Saved-file capture is unavailable"))?;
        state.users = state.users.saturating_sub(1);
        Ok(())
    }
}

impl Drop for SavedCapture {
    fn drop(&mut self) {
        if let Err(error) = self.release() {
            log::error!("Could not release saved-file capture: {error:#}");
        }
    }
}

#[derive(Clone)]
pub struct SavedSnapshot(Arc<SavedCaptureKind>);

enum SavedCaptureKind {
    Local(SavedCapture),
    Remote(crate::remote_client::RemoteSaved),
}

impl SavedSnapshot {
    pub fn capture(
        repository: &Repository,
        paths: impl IntoIterator<Item = String>,
    ) -> Result<Self> {
        if repository.is_remote() {
            return Ok(Self(Arc::new(SavedCaptureKind::Remote(
                crate::remote_client::RemoteSaved::capture(
                    repository,
                    paths.into_iter().collect(),
                )?,
            ))));
        }

        let files: HashMap<_, _> = paths
            .into_iter()
            .map(|path| Ok((path.clone(), SavedFile::read(repository.root(), &path)?)))
            .collect::<Result<_>>()?;
        let mut waves = WAVES
            .lock()
            .map_err(|_| anyhow::anyhow!("Saved-file captures are unavailable"))?;
        waves.retain(|_, wave| wave.strong_count() > 0);
        let wave = waves.get(repository.root()).and_then(Weak::upgrade);
        let wave = match wave {
            Some(wave)
                if wave
                    .state
                    .lock()
                    .map_err(|_| anyhow::anyhow!("Saved-file capture is unavailable"))?
                    .users
                    > 0 =>
            {
                wave
            }
            _ => {
                let wave = Arc::new(SaveWave {
                    state: Mutex::new(SaveWaveState {
                        users: 0,
                        version: NEXT_VERSION.fetch_add(1, Ordering::Relaxed),
                        saved: HashMap::new(),
                        failure: None,
                        pending: HashMap::new(),
                    }),
                });
                waves.insert(repository.root().to_owned(), Arc::downgrade(&wave));
                wave
            }
        };
        drop(waves);
        let version = {
            let mut state = wave
                .state
                .lock()
                .map_err(|_| anyhow::anyhow!("Saved-file capture is unavailable"))?;
            for (path, file) in &files {
                state
                    .saved
                    .entry(path.clone())
                    .or_insert_with(|| file.clone());
            }

            state.users += 1;
            state.version
        };
        Ok(Self(Arc::new(SavedCaptureKind::Local(SavedCapture {
            repository: repository.clone(),
            wave,
            files,
            version,
            released: AtomicBool::new(false),
        }))))
    }

    pub fn version(&self) -> Option<u64> {
        match self.0.as_ref() {
            SavedCaptureKind::Local(capture) => Some(capture.version),
            SavedCaptureKind::Remote(capture) => capture.version(),
        }
    }
    pub fn files(&self) -> Result<&HashMap<String, SavedFile>> {
        match self.0.as_ref() {
            SavedCaptureKind::Local(capture) => Ok(&capture.files),
            SavedCaptureKind::Remote(capture) => capture.files(),
        }
    }

    pub(crate) fn root(&self) -> &Path {
        match self.0.as_ref() {
            SavedCaptureKind::Local(capture) => capture.repository.root(),
            SavedCaptureKind::Remote(capture) => capture.root(),
        }
    }

    pub(crate) async fn remote_id(
        &self,
        repository: &Repository,
    ) -> Result<crate::remote::ResourceId> {
        match self.0.as_ref() {
            SavedCaptureKind::Remote(capture) => capture.resolve(repository).await,
            SavedCaptureKind::Local(_) => {
                anyhow::bail!("Local saved files cannot be classified remotely")
            }
        }
    }

    pub async fn verify(&self) -> Result<()> {
        let capture = match self.0.as_ref() {
            SavedCaptureKind::Local(capture) => capture,
            SavedCaptureKind::Remote(capture) => return capture.verify(),
        };
        ensure!(
            !capture.released.load(Ordering::Acquire),
            "The saved-file capture was already released"
        );
        loop {
            let pending = {
                let state = capture
                    .wave
                    .state
                    .lock()
                    .map_err(|_| anyhow::anyhow!("Saved-file capture is unavailable"))?;
                ensure!(
                    state.failure.is_none(),
                    "Could not record a successful save: {}",
                    state.failure.as_deref().unwrap_or_default()
                );
                if state.pending.is_empty() {
                    // Starting a save takes this same lock before any disk write can run.
                    for (path, original) in &capture.files {
                        let expected = state.saved.get(path).unwrap_or(original);
                        ensure!(
                            &SavedFile::read(capture.repository.root(), path)? == expected,
                            "The file changed outside a recorded save while classification was pending: {path}"
                        );
                    }

                    return Ok(());
                }

                state.pending.values().cloned().collect::<Vec<_>>()
            };
            futures::future::join_all(pending).await;
        }
    }

    pub async fn finish(&self) -> Result<Option<Snapshot>> {
        let capture = match self.0.as_ref() {
            SavedCaptureKind::Local(capture) => capture,
            SavedCaptureKind::Remote(capture) => return capture.finish().await,
        };
        if !capture.release_after_saves().await? {
            return Ok(None);
        }

        let repository = &capture.repository;
        let _guard = crate::transaction::write_lock(repository.root()).await?;
        if is_deferred(repository.root())? {
            return Ok(None);
        }

        repository.check_lifetime()?;
        repository.snapshot_unlocked(true).await.map(Some)
    }

    pub(crate) async fn combined_files(snapshots: &[Self]) -> Result<HashMap<String, SavedFile>> {
        let mut files = HashMap::new();
        for snapshot in snapshots {
            snapshot.verify().await?;
            for (path, file) in snapshot.files()? {
                if let Some(previous) = files.insert(path.clone(), file.clone()) {
                    ensure!(
                        previous == *file,
                        "Different saved versions of {path} cannot share a classification batch"
                    );
                }
            }
        }

        Ok(files)
    }
}

pub fn record_saved_file(root: &Path, path: &str) -> Result<()> {
    let wave = WAVES
        .lock()
        .map_err(|_| anyhow::anyhow!("Saved-file captures are unavailable"))?
        .get(root)
        .and_then(Weak::upgrade);
    let Some(wave) = wave else {
        return Ok(());
    };

    if wave
        .state
        .lock()
        .map_err(|_| anyhow::anyhow!("Saved-file capture is unavailable"))?
        .users
        == 0
    {
        return Ok(());
    }

    let file = SavedFile::read(root, path);
    let mut state = wave
        .state
        .lock()
        .map_err(|_| anyhow::anyhow!("Saved-file capture is unavailable"))?;
    match file {
        Ok(file) => {
            state.saved.insert(path.into(), file);
            state.version = NEXT_VERSION.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
        Err(error) => {
            state.failure = Some(format!("{error:#}"));
            Err(error)
        }
    }
}

pub(crate) fn is_deferred(root: &Path) -> Result<bool> {
    let wave = WAVES
        .lock()
        .map_err(|_| anyhow::anyhow!("Saved-file captures are unavailable"))?
        .get(root)
        .and_then(Weak::upgrade);
    match wave {
        Some(wave) => Ok(wave
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("Saved-file capture is unavailable"))?
            .users
            > 0),
        None => Ok(false),
    }
}

#[derive(Deserialize)]
struct FileMode {
    kind: String,
    executable: bool,
}

impl Snapshot {
    async fn captured_file_mode(&self, revision: &str, path: &str) -> Result<Option<FileMode>> {
        let fileset = serde_json::to_string(&literal_fileset(path)?)?;
        let template = format!(
            r#""[" ++ self.files({fileset}).map(|file| "{{\"kind\":" ++ json(file.file_type()) ++ ",\"executable\":" ++ json(file.executable()) ++ "}}").join(",") ++ "]""#
        );
        let bytes = self
            .read(&[
                "log".into(),
                "-r".into(),
                revision.into(),
                "--no-graph".into(),
                "-T".into(),
                template,
            ])
            .await?;
        let mut modes: Vec<FileMode> = serde_json::from_slice(&bytes)?;
        ensure!(
            modes.len() <= 1,
            "Saved file does not identify one repository entry"
        );
        Ok(modes.pop())
    }
}

fn matches_saved_bytes(recorded: Option<&[u8]>, saved: Option<&[u8]>) -> bool {
    recorded == saved
        || recorded
            .and_then(crate::LfsPointer::parse)
            .is_some_and(|pointer| {
                saved.is_some_and(|bytes| {
                    pointer.size == bytes.len() as u64
                        && pointer.hash == format!("{:x}", Sha256::digest(bytes))
                })
            })
}

impl Transaction {
    pub(crate) async fn prepare_saved_files(
        &mut self,
        files: &HashMap<String, SavedFile>,
    ) -> Result<bool> {
        let before = self.snapshot().clone();
        let mut changed = false;
        for (path, saved) in files {
            let original = before.file_content_if_exists("@", path).await?;
            let mode = before.captured_file_mode("@", path).await?;
            ensure!(
                mode.as_ref().is_none_or(|mode| mode.kind == "file"),
                "Classification requires a regular, conflict-free file: {path}"
            );
            if matches_saved_bytes(original.as_deref(), saved.bytes.as_deref())
                && mode
                    .as_ref()
                    .is_none_or(|mode| mode.executable == saved.executable)
            {
                continue;
            }

            if saved.bytes.is_none() && original.is_some() {
                let patterns = before.read(&["sparse".into(), "list".into()]).await?;
                let patterns = String::from_utf8(patterns)?;
                if !patterns.lines().any(|pattern| {
                    pattern == "." || path == pattern || path.starts_with(&format!("{pattern}/"))
                }) {
                    continue;
                }
            }

            changed = true;
            let Some(bytes) = &saved.bytes else {
                self.prepare(&[
                    "file",
                    "delete",
                    "-r",
                    "@",
                    "--restore-descendants",
                    "--",
                    &literal_fileset(path)?,
                ])
                .await?;
                continue;
            };
            let directory =
                smol::unblock(|| tempfile::Builder::new().prefix("zed-kk-saved-").tempdir())
                    .await?;
            let input = directory.path().join("input");
            smol::fs::write(&input, bytes).await?;
            #[cfg(not(windows))]
            let (program, script, contents) = (
                "sh",
                directory.path().join("editor.sh"),
                "set -eu\nmkdir -p -- \"$(dirname -- \"$3\")\"\ncp -- \"$1\" \"$3\"\nchmod \"$2\" \"$3\"\n",
            );
            #[cfg(windows)]
            let (program, script, contents) = (
                "powershell.exe",
                directory.path().join("editor.ps1"),
                "param($inputFile, $mode, $targetFile)\n$ErrorActionPreference = 'Stop'\n[System.IO.Directory]::CreateDirectory([System.IO.Path]::GetDirectoryName($targetFile)) | Out-Null\n[System.IO.File]::Copy($inputFile, $targetFile, $true)\n",
            );
            smol::fs::write(&script, contents).await?;
            let mut editor = vec![program];
            #[cfg(windows)]
            editor.extend(["-NoProfile", "-NonInteractive", "-File"]);
            editor.extend([
                script.to_str().context("Temporary path is not UTF-8")?,
                input.to_str().context("Temporary path is not UTF-8")?,
                if saved.executable { "755" } else { "644" },
            ]);
            let configuration = format!("ui.editor={}", serde_json::to_string(&editor)?);
            let result = self
                .prepare(&[
                    "file",
                    "edit",
                    "-r",
                    "@",
                    "--restore-descendants",
                    "--config",
                    &configuration,
                    "--",
                    path,
                ])
                .await;
            smol::unblock(move || directory.close()).await?;
            result?;
            let actual = self.snapshot().file_content_if_exists("@", path).await?;
            ensure!(
                matches_saved_bytes(actual.as_deref(), saved.bytes.as_deref()),
                "KK did not preserve the captured bytes for {path}. Nothing was published."
            );
            let actual_mode = self
                .snapshot()
                .captured_file_mode("@", path)
                .await?
                .context("Prepared input is missing")?;
            ensure!(
                actual_mode.executable == saved.executable,
                "KK did not preserve the captured executable bit for {path}. Nothing was published."
            );
        }

        Ok(changed)
    }
}
