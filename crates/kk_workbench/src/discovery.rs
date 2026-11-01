use crate::{Diagnostics, PerformanceContext, Repository};
use smol::stream::StreamExt as _;
use std::{
    collections::{HashSet, VecDeque},
    path::{Path, PathBuf},
};

#[derive(Debug)]
pub struct DiscoveryError {
    pub path: PathBuf,
    pub message: String,
}

#[derive(Debug, Default)]
pub struct Discovery {
    pub repositories: Vec<Repository>,
    pub errors: Vec<DiscoveryError>,
}

pub async fn discover(directory: &Path, executable: &Path) -> Discovery {
    discover_with_diagnostics(directory, executable, Diagnostics::default(), None).await
}

pub async fn discover_with_diagnostics(
    directory: &Path,
    executable: &Path,
    diagnostics: Diagnostics,
    context: Option<PerformanceContext>,
) -> Discovery {
    let mut result = Discovery::default();
    let mut queue = VecDeque::from([directory.to_owned()]);
    let mut visited = HashSet::new();
    while let Some(directory) = queue.pop_front() {
        let canonical = match smol::fs::canonicalize(&directory).await {
            Ok(path) => path,
            Err(error) => {
                result.errors.push(DiscoveryError {
                    path: directory,
                    message: error.to_string(),
                });
                continue;
            }
        };
        if canonical
            .components()
            .any(|component| matches!(component.as_os_str().to_str(), Some(".kk" | ".jj" | ".git")))
            || !visited.insert(canonical.clone())
        {
            continue;
        }

        let mut has_repository = false;
        for ancestor in canonical.ancestors() {
            match smol::fs::metadata(ancestor.join(".kk")).await {
                Ok(_) => {
                    has_repository = true;
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    result.errors.push(DiscoveryError {
                        path: ancestor.to_owned(),
                        message: error.to_string(),
                    });
                    break;
                }
            }
        }

        if has_repository {
            match Repository::open_with_diagnostics(
                &canonical,
                executable,
                diagnostics.clone(),
                context.clone(),
            )
            .await
            {
                Ok(repository) => {
                    if !result
                        .repositories
                        .iter()
                        .any(|existing| existing.root() == repository.root())
                    {
                        result.repositories.push(repository);
                    }

                    continue;
                }
                Err(error) => result.errors.push(DiscoveryError {
                    path: canonical.clone(),
                    message: format!("{error:#}"),
                }),
            }
        }

        let mut entries = match smol::fs::read_dir(&canonical).await {
            Ok(entries) => entries,
            Err(error) => {
                result.errors.push(DiscoveryError {
                    path: canonical,
                    message: error.to_string(),
                });
                continue;
            }
        };
        let mut children = Vec::new();
        while let Some(entry) = entries.next().await {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    result.errors.push(DiscoveryError {
                        path: canonical.clone(),
                        message: error.to_string(),
                    });
                    continue;
                }
            };
            if matches!(entry.file_name().to_str(), Some(".kk" | ".jj" | ".git")) {
                continue;
            }

            let path = entry.path();
            match smol::fs::metadata(&path).await {
                Ok(metadata) if metadata.is_dir() => children.push(path),
                Ok(_) => {}
                Err(error) => result.errors.push(DiscoveryError {
                    path,
                    message: error.to_string(),
                }),
            }
        }

        children.sort();
        queue.extend(children);
    }

    let roots: Vec<_> = result
        .repositories
        .iter()
        .map(|repository| repository.root().to_owned())
        .collect();
    result.repositories.retain(|repository| {
        !roots
            .iter()
            .any(|root| root != repository.root() && repository.root().starts_with(root))
    });
    result
        .repositories
        .sort_by(|left, right| left.root().cmp(right.root()));

    result
}
