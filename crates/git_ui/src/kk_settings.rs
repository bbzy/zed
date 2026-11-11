use db::kvp::KeyValueStore;
use futures::{FutureExt as _, future::Shared};
use gpui::{App, AppContext as _, Context, Task};
use settings::{RegisterSetting, Settings};
use std::{path::PathBuf, sync::Arc};
use util::ResultExt as _;

pub(super) struct HistoryPreference {
    pub mode: kk_workbench::HistoryMode,
    store: Arc<KeyValueStore>,
    key: Option<String>,
    writes: Shared<Task<()>>,
}

impl HistoryPreference {
    pub fn new(workspace_id: Option<String>, cx: &App) -> anyhow::Result<Self> {
        Self::load(KeyValueStore::global(cx).into(), workspace_id)
    }

    fn load(store: Arc<KeyValueStore>, workspace_id: Option<String>) -> anyhow::Result<Self> {
        let key = workspace_id.map(|identifier| format!("KKWorkbenchHistoryMode:{identifier}"));
        let mode = key
            .as_ref()
            .map(|key| store.read_kvp(key))
            .transpose()?
            .flatten()
            .and_then(|value| serde_json::from_str(&value).ok())
            .unwrap_or_default();
        Ok(Self {
            mode,
            store,
            key,
            writes: Task::ready(()).shared(),
        })
    }

    pub fn set<T: 'static>(
        &mut self,
        mode: kk_workbench::HistoryMode,
        cx: &mut Context<T>,
        on_error: impl FnOnce(&mut T, String, &mut Context<T>) + 'static,
    ) {
        self.mode = mode;
        let Some(key) = self.key.clone() else {
            return;
        };
        let store = self.store.clone();
        let previous = self.writes.clone();
        self.writes = cx
            .spawn(async move |this, cx| {
                previous.await;
                let result = cx
                    .background_spawn(async move {
                        store.write_kvp(key, serde_json::to_string(&mode)?).await
                    })
                    .await;
                if let Err(error) = result {
                    log::error!("Could not save KK history mode: {error:#}");
                    this.update(cx, |this, cx| {
                        on_error(this, format!("Could not save history mode: {error:#}"), cx)
                    })
                    .log_err();
                }
            })
            .shared();
        let write = self.writes.clone();
        cx.spawn(async move |_, _| write.await).detach();
    }
}

pub(super) struct CommitInformation {
    visible: bool,
    store: Arc<KeyValueStore>,
    key: Option<String>,
    writes: Shared<Task<()>>,
    pub error: Option<String>,
}

impl CommitInformation {
    pub fn new(workspace_id: Option<String>, cx: &App) -> anyhow::Result<Self> {
        let store: Arc<KeyValueStore> = KeyValueStore::global(cx).into();
        Self::load(store, workspace_id)
    }

    fn load(store: Arc<KeyValueStore>, workspace_id: Option<String>) -> anyhow::Result<Self> {
        let key =
            workspace_id.map(|identifier| format!("KKWorkbenchCommitInformation:{identifier}"));
        let visible = key
            .as_ref()
            .map(|key| store.read_kvp(key))
            .transpose()?
            .flatten()
            .is_none_or(|value| value != "hidden");
        Ok(Self {
            visible,
            store,
            key,
            writes: Task::ready(()).shared(),
            error: None,
        })
    }

    pub fn visible(&self) -> bool {
        self.visible
    }

    pub fn toggle(&mut self, cx: &mut Context<Self>) {
        self.visible = !self.visible;
        self.error = None;
        cx.notify();
        let Some(key) = self.key.clone() else {
            return;
        };
        let value = if self.visible { "visible" } else { "hidden" }.to_owned();
        let store = self.store.clone();
        let previous = self.writes.clone();
        self.writes = cx
            .spawn(async move |this, cx| {
                previous.await;
                let result = cx
                    .background_spawn(async move { store.write_kvp(key, value).await })
                    .await;
                if let Err(error) = result {
                    log::error!("Failed to save KK commit information preference: {error:#}");
                    this.update(cx, |this, cx| {
                        this.error = Some(format!(
                            "Failed to save commit information preference: {error:#}"
                        ));
                        cx.notify();
                    })
                    .log_err();
                }
            })
            .shared();
        let write = self.writes.clone();
        cx.spawn(async move |_, _| write.await).detach();
    }
}

pub(super) struct DiffDisplayPreference {
    showing_full_file: bool,
    store: Arc<KeyValueStore>,
    key: Option<String>,
    writes: Shared<Task<()>>,
    pub error: Option<String>,
}

impl DiffDisplayPreference {
    pub fn new(workspace_id: Option<String>, cx: &App) -> anyhow::Result<Self> {
        let store: Arc<KeyValueStore> = KeyValueStore::global(cx).into();
        Self::load(
            store,
            workspace_id,
            editor::EditorSettings::get_global(cx)
                .file_diff
                .show_full_file,
        )
    }

    fn load(
        store: Arc<KeyValueStore>,
        workspace_id: Option<String>,
        default: bool,
    ) -> anyhow::Result<Self> {
        let key =
            workspace_id.map(|identifier| format!("KKWorkbenchDiffDisplayPreference:{identifier}"));
        let showing_full_file = key
            .as_ref()
            .map(|key| store.read_kvp(key))
            .transpose()?
            .flatten()
            .map_or(default, |value| value == "full");
        Ok(Self {
            showing_full_file,
            store,
            key,
            writes: Task::ready(()).shared(),
            error: None,
        })
    }

    pub fn showing_full_file(&self) -> bool {
        self.showing_full_file
    }

    pub fn toggle(&mut self, cx: &mut Context<Self>) {
        self.showing_full_file = !self.showing_full_file;
        self.error = None;
        cx.notify();
        let Some(key) = self.key.clone() else {
            return;
        };
        let value = if self.showing_full_file {
            "full"
        } else {
            "changes"
        }
        .to_owned();
        let store = self.store.clone();
        let previous = self.writes.clone();
        self.writes = cx
            .spawn(async move |this, cx| {
                previous.await;
                let result = cx
                    .background_spawn(async move { store.write_kvp(key, value).await })
                    .await;
                if let Err(error) = result {
                    log::error!("Failed to save KK diff display preference: {error:#}");
                    this.update(cx, |this, cx| {
                        this.error =
                            Some(format!("Failed to save diff display preference: {error:#}"));
                        cx.notify();
                    })
                    .log_err();
                }
            })
            .shared();
        let write = self.writes.clone();
        cx.spawn(async move |_, _| write.await).detach();
    }
}

#[derive(Clone, Debug, PartialEq, RegisterSetting)]
pub struct KkWorkbenchSettings {
    pub dock: workspace::dock::DockPosition,
    pub executable_path: PathBuf,
    pub auto_refresh: bool,
    pub blame: BlameSettings,
    pub debug: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BlameSettings {
    pub column_width: u32,
    pub compact: bool,
    pub show_status_bar: bool,
}

impl Settings for KkWorkbenchSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let content = content.kk_workbench.clone().unwrap_or_default();
        let blame = content.blame.unwrap_or_default();
        Self {
            dock: match content.dock {
                Some(settings::DockPosition::Left) => workspace::dock::DockPosition::Left,
                _ => workspace::dock::DockPosition::Right,
            },
            executable_path: content
                .executable_path
                .unwrap_or_else(|| "kk".into())
                .into(),
            auto_refresh: content.auto_refresh.unwrap_or(true),
            blame: BlameSettings {
                column_width: blame.column_width.unwrap_or(42).clamp(24, 80),
                compact: blame.compact.unwrap_or(false),
                show_status_bar: blame.show_status_bar.unwrap_or(true),
            },
            debug: content.debug.unwrap_or_default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[test]
    fn kk_dock_is_independent_and_survives_settings_round_trip() {
        use workspace::dock::DockPosition;

        let content: settings::SettingsContent =
            serde_json::from_str(r#"{"git_panel":{"dock":"left"}}"#).expect("settings");
        assert_eq!(
            KkWorkbenchSettings::from_settings(&content).dock,
            DockPosition::Right
        );

        for position in [settings::DockPosition::Left, settings::DockPosition::Right] {
            let mut content = content.clone();
            content.kk_workbench.get_or_insert_default().dock = Some(position);
            let serialized = serde_json::to_string(&content).expect("serialize");
            let restored = serde_json::from_str(&serialized).expect("restore");
            assert_eq!(
                KkWorkbenchSettings::from_settings(&restored).dock,
                position.into()
            );
            assert_eq!(
                content.git_panel.expect("git settings").dock,
                Some(settings::DockPosition::Left)
            );
        }
    }

    #[gpui::test]
    async fn kk_history_mode_survives_queued_switches_and_isolates_workspaces(
        cx: &mut TestAppContext,
    ) {
        use kk_workbench::HistoryMode;
        let store = Arc::new(KeyValueStore::open_test_db("kk_history_mode").await);
        let preference =
            HistoryPreference::load(store.clone(), Some("first".into())).expect("load");
        assert_eq!(preference.mode, HistoryMode::Lineage);
        let preference = cx.new(|_| preference);
        let written = preference.update(cx, |preference, cx| {
            preference.set(HistoryMode::Log, cx, |_, error, _| panic!("{error}"));
            preference.set(HistoryMode::Lineage, cx, |_, error, _| panic!("{error}"));
            preference.set(HistoryMode::Log, cx, |_, error, _| panic!("{error}"));
            preference.writes.clone()
        });
        drop(preference);
        written.await;
        assert_eq!(
            HistoryPreference::load(store.clone(), Some("first".into()))
                .expect("restore")
                .mode,
            HistoryMode::Log
        );
        assert_eq!(
            HistoryPreference::load(store.clone(), Some("second".into()))
                .expect("other workspace")
                .mode,
            HistoryMode::Lineage
        );
        assert_eq!(
            HistoryPreference::load(store, None)
                .expect("transient workspace")
                .mode,
            HistoryMode::Lineage
        );
    }

    #[gpui::test]
    async fn kk_diff_display_is_persistent_and_workspace_scoped(cx: &mut TestAppContext) {
        let store = Arc::new(KeyValueStore::open_test_db("kk_diff_display").await);
        let preference =
            DiffDisplayPreference::load(store.clone(), Some("first".into()), true).expect("load");
        let preference = cx.new(|_| preference);
        let writes = preference.update(cx, |preference, cx| {
            preference.toggle(cx);
            preference.toggle(cx);
            preference.toggle(cx);
            preference.writes.clone()
        });
        drop(preference);
        writes.await;
        assert!(
            !DiffDisplayPreference::load(store.clone(), Some("first".into()), true)
                .expect("restore")
                .showing_full_file()
        );
        assert!(
            DiffDisplayPreference::load(store.clone(), Some("second".into()), true)
                .expect("other workspace")
                .showing_full_file()
        );
        assert!(
            !DiffDisplayPreference::load(store, None, false)
                .expect("transient")
                .showing_full_file()
        );
    }

    #[gpui::test]
    async fn kk_commit_information_survives_queued_toggles_and_closing_the_project(
        cx: &mut TestAppContext,
    ) {
        let store = Arc::new(KeyValueStore::open_test_db("kk_commit_information").await);
        let preference =
            CommitInformation::load(store.clone(), Some("first".into())).expect("preference");
        let preference = cx.new(|_| preference);
        assert!(preference.read_with(cx, |preference, _| preference.visible()));
        let writes = preference.update(cx, |preference, cx| {
            preference.toggle(cx);
            preference.toggle(cx);
            preference.toggle(cx);
            assert!(!preference.visible());
            preference.writes.clone()
        });
        drop(preference);
        writes.await;
        let restored =
            CommitInformation::load(store.clone(), Some("first".into())).expect("restore");
        assert!(!restored.visible());
        let other =
            CommitInformation::load(store.clone(), Some("second".into())).expect("other project");
        assert!(other.visible());
        let transient = CommitInformation::load(store, None).expect("transient project");
        assert!(transient.visible());
    }
}
