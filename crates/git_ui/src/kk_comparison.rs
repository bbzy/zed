use db::kvp::KeyValueStore;
use futures::{FutureExt as _, future::Shared};
use gpui::{App, AppContext as _, Context, Task};
use kk_workbench::{CategoryCommit, ChangedFile, DiffScope, Revision, Snapshot};
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, path::Path, sync::Arc};
use util::ResultExt as _;

#[derive(Clone, Serialize, Deserialize)]
pub struct ComparisonIntent {
    version: u32,
    pub target_revset: String,
    pub target_label: String,
    pub range_start_change_id: Option<String>,
    #[serde(default)]
    pub range_start_display_id: Option<String>,
}

impl ComparisonIntent {
    pub(super) fn for_target(
        target_revset: String,
        target_label: String,
        range_start: Option<&str>,
    ) -> Self {
        Self {
            version: 1,
            target_revset,
            target_label,
            range_start_change_id: range_start.map(str::to_owned),
            range_start_display_id: None,
        }
    }

    pub fn decode(text: &str) -> Option<Self> {
        let intent: Self = serde_json::from_str(text).ok()?;
        (intent.version == 1
            && !intent.target_revset.trim().is_empty()
            && intent
                .range_start_change_id
                .as_ref()
                .is_none_or(|identifier| {
                    !identifier.is_empty()
                        && identifier.bytes().all(|byte| (b'k'..=b'z').contains(&byte))
                }))
        .then_some(intent)
    }
}

#[derive(Clone)]
pub struct Comparison {
    pub intent: ComparisonIntent,
    pub files: Vec<ChangedFile>,
    pub categories: Vec<CategoryCommit>,
    pub sources: HashSet<String>,
    base_revset: String,
}

impl Comparison {
    pub fn new(revision: &Revision) -> Self {
        Self::pending(ComparisonIntent {
            version: 1,
            target_revset: revision.revset(),
            target_label: format!("{} {}", revision.change_id, revision.subject()),
            range_start_change_id: None,
            range_start_display_id: None,
        })
    }

    pub fn pending(intent: ComparisonIntent) -> Self {
        Self {
            base_revset: intent.target_revset.clone(),
            intent,
            files: Vec::new(),
            categories: Vec::new(),
            sources: HashSet::new(),
        }
    }

    pub async fn load(snapshot: &Snapshot, mut intent: ComparisonIntent) -> anyhow::Result<Self> {
        let boundary = snapshot
            .comparison_boundary(
                &intent.target_revset,
                intent.range_start_change_id.as_deref(),
            )
            .await?;
        let categories = if let Some(range_start) = &boundary.range_start {
            snapshot.category_commits(range_start).await?
        } else {
            Vec::new()
        };
        let mut sources = snapshot
            .revisions(Some(&format!("({})..@", boundary.base_revset)), usize::MAX)
            .await?
            .into_iter()
            .map(|revision| revision.full_commit_id)
            .collect::<HashSet<_>>();
        sources.extend(
            categories
                .iter()
                .map(|category| category.revision.full_commit_id.clone()),
        );
        intent.range_start_display_id = boundary
            .range_start
            .as_ref()
            .map(|revision| revision.change_id.clone());
        intent.range_start_change_id = boundary.range_start.map(|revision| revision.full_change_id);
        let mut comparison = Self {
            intent,
            files: Vec::new(),
            categories,
            sources,
            base_revset: boundary.base_revset,
        };
        comparison.files = snapshot.changed_files(&comparison.scope()).await?;
        Ok(comparison)
    }

    pub fn scope(&self) -> DiffScope {
        DiffScope::Comparison {
            base: self.base_revset.clone(),
            target: "@".into(),
        }
    }

    pub fn diff_target(&self) -> super::kk_diff::DiffTarget {
        let mut target = super::kk_diff::DiffTarget::comparison(
            self.intent.target_revset.clone(),
            self.base_revset.clone(),
            self.intent.target_label.clone(),
            self.intent.range_start_change_id.as_deref(),
        );
        target.comparison = Some(self.intent.clone());
        target
    }
}

pub struct ComparisonState {
    store: Arc<KeyValueStore>,
    writes: Shared<Task<()>>,
    host: Option<String>,
}

impl ComparisonState {
    pub fn new(host: Option<String>, cx: &App) -> Self {
        Self {
            store: KeyValueStore::global(cx).into(),
            writes: Task::ready(()).shared(),
            host,
        }
    }

    pub fn reader(&self) -> (Arc<KeyValueStore>, Shared<Task<()>>, Option<String>) {
        (self.store.clone(), self.writes.clone(), self.host.clone())
    }

    pub fn key(root: &Path, host: Option<&str>) -> String {
        match host {
            Some(host) => format!(
                "KKWorkbenchRemoteComparison:{}",
                serde_json::json!([host, root.to_string_lossy()])
            ),
            None => format!("KKWorkbenchComparison:{}", root.display()),
        }
    }

    pub fn save<T: 'static>(
        &mut self,
        root: &Path,
        intent: Option<&ComparisonIntent>,
        cx: &mut Context<T>,
        on_error: impl FnOnce(&mut T, String, &mut Context<T>) + 'static,
    ) {
        let value = intent.map(serde_json::to_string).transpose();
        let store = self.store.clone();
        let key = Self::key(root, self.host.as_deref());
        let previous = self.writes.clone();
        self.writes = cx
            .spawn(async move |this, cx| {
                previous.await;
                let result = cx
                    .background_spawn(async move {
                        match value? {
                            Some(value) => store.write_kvp(key, value).await,
                            None => store.delete_kvp(key).await,
                        }
                    })
                    .await;
                if let Err(error) = result {
                    log::error!("Failed to save KK comparison: {error:#}");
                    this.update(cx, |this, cx| {
                        on_error(this, format!("Failed to save comparison: {error:#}"), cx)
                    })
                    .log_err();
                }
            })
            .shared();
        let write = self.writes.clone();
        // A closed panel must not cancel a queued close and resurrect an earlier comparison.
        cx.spawn(async move |_, _| write.await).detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    struct StateOwner {
        state: ComparisonState,
        errors: Vec<String>,
    }

    impl StateOwner {
        fn error(&mut self, error: String, _: &mut Context<Self>) {
            self.errors.push(error);
        }
    }

    #[gpui::test]
    async fn comparison_writes_preserve_latest_intent_and_close_when_panel_drops(
        cx: &mut TestAppContext,
    ) {
        let store = Arc::new(KeyValueStore::open_test_db("kk_comparison_order").await);
        let owner = cx.new(|_| StateOwner {
            state: ComparisonState {
                store: store.clone(),
                writes: Task::ready(()).shared(),
                host: None,
            },
            errors: Vec::new(),
        });
        let first = ComparisonIntent {
            version: 1,
            target_revset: "change_id(kkkk)".into(),
            target_label: "first comparison".into(),
            range_start_change_id: Some("llll".into()),
            range_start_display_id: None,
        };
        let second = ComparisonIntent {
            target_label: "newer comparison".into(),
            ..first.clone()
        };
        let flush = owner.update(cx, |owner, cx| {
            owner
                .state
                .save(Path::new("/first"), Some(&first), cx, StateOwner::error);
            owner
                .state
                .save(Path::new("/first"), Some(&second), cx, StateOwner::error);
            owner
                .state
                .save(Path::new("/second"), Some(&second), cx, StateOwner::error);
            owner
                .state
                .save(Path::new("/first"), None, cx, StateOwner::error);
            owner.state.reader().1
        });
        drop(owner);
        flush.await;
        assert!(
            store
                .read_kvp(&ComparisonState::key(Path::new("/first"), None))
                .expect("read first repository")
                .is_none()
        );
        let saved = store
            .read_kvp(&ComparisonState::key(Path::new("/second"), None))
            .expect("read second repository")
            .expect("second repository must remain saved");
        let restored = ComparisonIntent::decode(&saved).expect("restore comparison intent");
        assert_eq!(restored.target_label, "newer comparison");
        assert_eq!(restored.range_start_change_id.as_deref(), Some("llll"));
        assert!(ComparisonIntent::decode("broken json").is_none());
        assert!(
            ComparisonIntent::decode(&saved.replace("\"version\":1", "\"version\":99")).is_none()
        );
    }
}
