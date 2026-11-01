use db::kvp::KeyValueStore;
use futures::{FutureExt as _, future::Shared};
use gpui::{App, AppContext as _, Context, Task};
use icu_collator::{
    Collator, CollatorBorrowed, CollatorPreferences,
    options::{CollatorOptions, Strength},
    preferences::CollationNumericOrdering,
};
use icu_locale_core::Locale;
use kk_workbench::ChangedFile;
use std::sync::Arc;
use util::ResultExt as _;

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub enum FileOrder {
    #[default]
    Repository,
    FileName,
}

pub struct FileOrdering {
    order: FileOrder,
    collator: CollatorBorrowed<'static>,
    store: Arc<KeyValueStore>,
    key: Option<String>,
    writes: Shared<Task<()>>,
}

impl FileOrdering {
    pub fn new(workspace_id: Option<String>, cx: &App) -> anyhow::Result<Self> {
        let locale = sys_locale::get_locale()
            .and_then(|locale| locale.parse::<Locale>().log_err())
            .unwrap_or(Locale::UNKNOWN);
        let store: Arc<KeyValueStore> = KeyValueStore::global(cx).into();
        let key = workspace_id.map(|identifier| format!("KKWorkbenchFileOrder:{identifier}"));
        let order = match key.as_ref().map(|key| store.read_kvp(key)).transpose()? {
            Some(Some(value)) if value == "fileName" => FileOrder::FileName,
            _ => FileOrder::Repository,
        };
        Ok(Self {
            order,
            collator: Self::collator(locale)?,
            store,
            key,
            writes: Task::ready(()).shared(),
        })
    }

    fn collator(locale: Locale) -> anyhow::Result<CollatorBorrowed<'static>> {
        let mut preferences: CollatorPreferences = locale.into();
        preferences.numeric_ordering = Some(CollationNumericOrdering::True);
        let mut options = CollatorOptions::default();
        options.strength = Some(Strength::Primary);
        Ok(Collator::try_new(preferences, options)?)
    }

    pub fn order(&self) -> FileOrder {
        self.order
    }

    pub fn files<'a>(&self, files: &'a [ChangedFile]) -> Vec<&'a ChangedFile> {
        let mut ordered = files.iter().collect::<Vec<_>>();
        if self.order == FileOrder::FileName {
            ordered.sort_by(|left, right| self.compare(&left.path, &right.path));
        }

        ordered
    }

    pub fn compare(&self, left: &str, right: &str) -> std::cmp::Ordering {
        self.collator
            .compare(
                left.rsplit('/').next().unwrap_or(left),
                right.rsplit('/').next().unwrap_or(right),
            )
            .then_with(|| left.split('/').cmp(right.split('/')))
    }

    pub fn set<T: 'static>(
        &mut self,
        order: FileOrder,
        cx: &mut Context<T>,
        on_error: impl FnOnce(&mut T, String, &mut Context<T>) + 'static,
    ) {
        if self.order == order {
            return;
        }

        self.order = order;
        cx.notify();
        let Some(key) = self.key.clone() else {
            return;
        };
        let value = match order {
            FileOrder::Repository => "repository",
            FileOrder::FileName => "fileName",
        };
        let store = self.store.clone();
        let previous = self.writes.clone();
        self.writes = cx
            .spawn(async move |this, cx| {
                previous.await;
                let result = cx
                    .background_spawn(async move { store.write_kvp(key, value.to_owned()).await })
                    .await;
                if let Err(error) = result {
                    log::error!("Failed to save KK file order: {error:#}");
                    this.update(cx, |this, cx| {
                        on_error(this, format!("Failed to save file order: {error:#}"), cx)
                    })
                    .log_err();
                }
            })
            .shared();
        let write = self.writes.clone();
        // Closing the panel must retain the last selected order, including queued switches.
        cx.spawn(async move |_, _| write.await).detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use kk_workbench::ChangeKind;

    #[gpui::test]
    async fn filename_order_respects_locale_numeric_names_and_repository_path_ties(
        cx: &mut TestAppContext,
    ) {
        let store = Arc::new(KeyValueStore::open_test_db("kk_file_order").await);
        let mut ordering = FileOrdering {
            order: FileOrder::FileName,
            collator: FileOrdering::collator("en-US".parse().expect("English locale"))
                .expect("collator"),
            store,
            key: None,
            writes: Task::ready(()).shared(),
        };
        let paths = [
            "b/z2.txt",
            "b/file10.txt",
            "z/file2.txt",
            "a/FILE2.txt",
            "a-é/x.txt",
            "a/z/x.txt",
            "a/z10.txt",
            "a/e\u{301}.txt",
            "b/é.txt",
        ];
        let files = paths
            .iter()
            .map(|path| ChangedFile {
                path: (*path).to_owned(),
                change_type: ChangeKind::Modified,
                renamed_from: None,
                has_conflict: false,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            ordering
                .files(&files)
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            [
                "a/e\u{301}.txt",
                "b/é.txt",
                "a/FILE2.txt",
                "z/file2.txt",
                "b/file10.txt",
                "a/z/x.txt",
                "a-é/x.txt",
                "b/z2.txt",
                "a/z10.txt"
            ]
        );
        assert!(ordering.compare("ä.txt", "z.txt").is_lt());
        ordering.collator =
            FileOrdering::collator("sv-SE".parse().expect("Swedish locale")).expect("collator");
        assert!(ordering.compare("ä.txt", "z.txt").is_gt());
        ordering.order = FileOrder::Repository;
        assert_eq!(
            ordering
                .files(&files)
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            paths
        );
        cx.run_until_parked();
    }

    #[gpui::test]
    async fn latest_workspace_order_survives_queued_switches_and_panel_close(
        cx: &mut TestAppContext,
    ) {
        let store = Arc::new(KeyValueStore::open_test_db("kk_file_order_writes").await);
        let ordering = cx.new(|_| FileOrdering {
            order: FileOrder::Repository,
            collator: FileOrdering::collator(Locale::UNKNOWN).expect("collator"),
            store: store.clone(),
            key: Some("KKWorkbenchFileOrder:1".into()),
            writes: Task::ready(()).shared(),
        });
        let written = ordering.update(cx, |ordering, cx| {
            ordering.set(FileOrder::FileName, cx, |_, _, _| {});
            ordering.set(FileOrder::Repository, cx, |_, _, _| {});
            ordering.writes.clone()
        });
        drop(ordering);
        written.await;
        assert_eq!(
            store
                .read_kvp("KKWorkbenchFileOrder:1")
                .expect("read saved order")
                .as_deref(),
            Some("repository")
        );
    }
}
