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

        let mut repository_root = None;
        for ancestor in canonical.ancestors() {
            match smol::fs::metadata(ancestor.join(".kk")).await {
                Ok(_) => {
                    repository_root = Some(ancestor.to_owned());
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

        if let Some(root) = repository_root {
            if !result
                .repositories
                .iter()
                .any(|repository| repository.root() == root)
            {
                // Repository commands run later so an unresponsive CLI cannot block discovery of its siblings.
                result.repositories.push(Repository::discovered(
                    root,
                    executable,
                    diagnostics.clone(),
                    context.clone(),
                ));
            }
            continue;
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
