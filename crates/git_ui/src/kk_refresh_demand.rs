use super::*;
use gpui::AnyWindowHandle;

pub(super) struct RefreshDemand {
    pub version: u64,
    pub list_version: u64,
    pub running_version: u64,
    pub refresh_open_diffs: bool,
    pub snapshots: HashMap<PathBuf, (u64, Snapshot)>,
    window: AnyWindowHandle,
    items: HashMap<EntityId, u64>,
    watched: HashMap<EntityId, Subscription>,
    queued: bool,
    running: bool,
    pub(super) requested: bool,
    error: Option<String>,
    task: Task<()>,
}

impl RefreshDemand {
    pub fn new(window: AnyWindowHandle) -> Self {
        Self {
            version: 1,
            list_version: 0,
            running_version: 0,
            refresh_open_diffs: true,
            snapshots: HashMap::new(),
            window,
            items: HashMap::new(),
            watched: HashMap::new(),
            queued: false,
            running: false,
            requested: false,
            error: None,
            task: Task::ready(()),
        }
    }

    pub fn invalidate(&mut self) {
        self.version += 1;
    }
}

#[derive(Clone)]
enum FocusedDiff {
    Single(Entity<KkDiff>),
    Multiple(Entity<KkMultiDiff>),
}

impl FocusedDiff {
    fn id(&self) -> EntityId {
        match self {
            Self::Single(diff) => diff.entity_id(),
            Self::Multiple(diff) => diff.entity_id(),
        }
    }

    fn is_focused(&self, workspace: &Workspace, window: &Window, cx: &App) -> bool {
        workspace.panes().iter().any(|pane| {
            let pane = pane.read(cx);
            pane.has_focus(window, cx)
                && pane
                    .active_item()
                    .is_some_and(|item| item.item_id() == self.id())
        })
    }

    fn snapshot(&self, cx: &App) -> Option<Snapshot> {
        match self {
            Self::Single(diff) => diff.read(cx).snapshot().cloned(),
            Self::Multiple(diff) => Some(diff.read(cx).snapshot().clone()),
        }
    }

    fn refreshed(&self, snapshot: &Snapshot, cx: &App) -> bool {
        let error = match self {
            Self::Single(diff) => diff.read(cx).error(),
            Self::Multiple(diff) => diff.read(cx).error(),
        };
        error.is_none()
            && self.snapshot(cx).is_some_and(|current| {
                current.operation() == snapshot.operation()
                    && current.repository().check_lifetime().is_ok()
            })
    }

    fn refresh(&self, snapshot: Snapshot, cx: &mut App) -> Task<()> {
        match self {
            Self::Single(diff) => diff.update(cx, |diff, cx| diff.refresh_for_focus(snapshot, cx)),
            Self::Multiple(diff) => {
                diff.update(cx, |diff, cx| diff.refresh_for_focus(snapshot, cx))
            }
        }
    }
}

impl KkPanel {
    pub(super) fn invalidate_refresh(&mut self, cx: &mut Context<Self>) {
        self.refresh_demand.invalidate();
        self.schedule_focused_refresh(cx);
    }

    pub(super) fn schedule_focused_refresh(&mut self, cx: &mut Context<Self>) {
        if self.refresh_demand.running {
            self.refresh_demand.requested = true;
            return;
        }
        if self.refresh_demand.queued {
            return;
        }

        self.refresh_demand.queued = true;
        let panel = cx.weak_entity();
        let window = self.refresh_demand.window;
        // Item insertion and focus dispatch can happen while Workspace is being updated.
        cx.defer(move |cx| {
            window
                .update(cx, |_, window, cx| {
                    panel
                        .update(cx, |panel, cx| {
                            panel.refresh_demand.queued = false;
                            panel.refresh_focused(window, cx);
                        })
                        .log_err();
                })
                .log_err();
        });
    }

    fn refresh_focused(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let diffs = workspace
            .read(cx)
            .items_of_type::<KkDiff>(cx)
            .map(FocusedDiff::Single)
            .chain(
                workspace
                    .read(cx)
                    .items_of_type::<KkMultiDiff>(cx)
                    .map(FocusedDiff::Multiple),
            )
            .collect::<Vec<_>>();
        let open = diffs.iter().map(FocusedDiff::id).collect::<HashSet<_>>();
        self.refresh_demand.items.retain(|id, _| open.contains(id));
        let panes = workspace.read(cx).panes().to_vec();
        self.refresh_demand
            .watched
            .retain(|id, _| panes.iter().any(|pane| pane.entity_id() == *id));
        for pane in panes {
            if let std::collections::hash_map::Entry::Vacant(entry) =
                self.refresh_demand.watched.entry(pane.entity_id())
            {
                let subscription = cx.on_focus_in(&pane.focus_handle(cx), window, |this, _, cx| {
                    this.schedule_focused_refresh(cx)
                });
                entry.insert(subscription);
            }
        }
        if !window.is_window_active() {
            return;
        }
        if self.focus.contains_focused(window, cx) {
            if !self.loading && self.refresh_demand.list_version != self.refresh_demand.version {
                self.start_refresh_with_scope(false, cx);
            }
            return;
        }
        let Some(diff) = diffs
            .into_iter()
            .find(|diff| diff.is_focused(workspace.read(cx), window, cx))
        else {
            return;
        };
        if self.loading {
            if self.refresh_demand.refresh_open_diffs {
                self.refresh_demand.requested = true;
                return;
            }

            self.refresh_sequence += 1;
            self.refresh_task = Task::ready(());
            self.loading = false;
            self.refreshing_repositories.clear();
        }

        let version = self.refresh_demand.version;
        if self.refresh_demand.items.get(&diff.id()) == Some(&version) {
            return;
        }
        let Some(previous) = diff.snapshot(cx) else {
            return;
        };
        let root = previous.repository().root().to_owned();
        if !self.repository_is_open(&root, cx) {
            return;
        }

        let cached = self
            .refresh_demand
            .snapshots
            .get(&root)
            .filter(|(cached_version, snapshot)| {
                *cached_version == version && snapshot.repository().check_lifetime().is_ok()
            })
            .map(|(_, snapshot)| snapshot.clone());
        let project = workspace.read(cx).project().read(cx);
        let remote = project.kk_workbench_client(cx);
        let executable = KkWorkbenchSettings::get_global(cx).executable_path.clone();
        let diagnostics = self.output.read(cx).diagnostics().clone();
        self.refresh_demand.running = true;
        self.refresh_demand.requested = false;
        let window = self.refresh_demand.window;
        let workspace = self.workspace.clone();
        self.refresh_demand.task = cx.spawn(async move |this, cx| {
            let result = async {
                let snapshot = match cached {
                    Some(snapshot) => snapshot,
                    None => {
                        let root = root.clone();
                        cx.background_spawn(async move {
                            let repository = if let Some(remote) = remote {
                                remote.open(&root, &executable, diagnostics).await?
                            } else {
                                kk_workbench::Repository::open_with_diagnostics(
                                    &root,
                                    &executable,
                                    diagnostics,
                                    None,
                                )
                                .await?
                            };
                            repository.snapshot(true).await.map(|snapshot| {
                                snapshot.with_lifetime(previous.repository().lifetime().cloned())
                            })
                        })
                        .await?
                    }
                };
                let open = this.update(cx, |this, cx| {
                    if !this.repository_is_open(&root, cx) || this.refresh_demand.version != version
                    {
                        return false;
                    }

                    this.refresh_demand
                        .snapshots
                        .insert(root, (version, snapshot.clone()));
                    true
                })?;
                if !open {
                    return anyhow::Ok(false);
                }
                let task = window.update(cx, |_, window, cx| {
                    let workspace = workspace.upgrade()?;
                    (window.is_window_active() && diff.is_focused(workspace.read(cx), window, cx))
                        .then(|| diff.refresh(snapshot.clone(), cx))
                })?;
                let Some(task) = task else {
                    return anyhow::Ok(false);
                };
                task.await;
                let refreshed = cx.update(|cx| diff.refreshed(&snapshot, cx));
                if refreshed {
                    this.update(cx, |this, _| {
                        this.refresh_demand.items.insert(diff.id(), version);
                    })?;
                }
                anyhow::Ok(refreshed)
            }
            .await;
            this.update(cx, |this, cx| {
                this.refresh_demand.running = false;
                let successful = match result {
                    Ok(successful) => successful,
                    Err(error) => {
                        let message = format!("Could not refresh diff: {error:#}");
                        this.refresh_demand.error = Some(message.clone());
                        this.show_error(message, cx);
                        false
                    }
                };
                if successful
                    && let Some(error) = this.refresh_demand.error.take()
                    && this.error.as_ref() == Some(&error)
                {
                    this.error = None;
                    cx.notify();
                }
                if successful
                    || this.refresh_demand.requested
                    || this.refresh_demand.version != version
                {
                    this.schedule_focused_refresh(cx);
                }
            })
            .log_err();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::super::remote_tests::{RemoteFixture, run_kk};
    use super::*;
    use gpui::TestAppContext;
    use workspace::MultiWorkspace;

    #[gpui::test(iterations = 3)]
    async fn kk_refresh_demand_updates_only_the_focused_consumer(
        cx: &mut TestAppContext,
        server_cx: &mut TestAppContext,
    ) {
        let fixture = RemoteFixture::create(cx, server_cx).await.expect("fixture");
        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            MultiWorkspace::test_new(fixture.project.clone(), window, cx)
        });
        let workspace = multi_workspace.read_with(cx, |view, _| view.workspace().clone());
        let panel = cx
            .update(|window, cx| KkPanel::load(workspace.downgrade(), window.to_async(cx)))
            .await
            .expect("panel");
        let ordinary = workspace.update_in(cx, |workspace, window, cx| {
            <dyn fs::Fs>::set_global(fixture.project.read(cx).fs().clone(), cx);
            let editor = cx.new(|cx| editor::Editor::multi_line(window, cx));
            workspace.add_item(
                workspace.active_pane().clone(),
                Box::new(editor.clone()),
                None,
                true,
                true,
                window,
                cx,
            );
            window.activate_window();
            editor
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_panel(panel.clone(), window, cx)
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.run_until_parked();
        workspace.update(cx, |_, cx| {
            cx.emit(workspace::Event::TerminalFocusLost);
            cx.emit(workspace::Event::TerminalFocusLost);
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.refresh_sequence, 0,
                "ordinary editor must not load KK history"
            );
            assert!(panel.refresh_demand.version > panel.refresh_demand.list_version);
            assert!(!panel.loading);
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.focus_panel::<KkPanel>(window, cx);
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.run_until_parked();
        panel
            .update(cx, |panel, _| {
                std::mem::replace(&mut panel.refresh_task, Task::ready(()))
            })
            .await;
        let (snapshot, list_version, list_requests) = panel.read_with(cx, |panel, _| {
            assert!(panel.error.is_none(), "{:?}", panel.error);
            assert_eq!(
                panel.refresh_demand.version,
                panel.refresh_demand.list_version
            );
            (
                panel.repositories[0].snapshot.clone(),
                panel.refresh_demand.list_version,
                panel.refresh_sequence,
            )
        });
        let parent = snapshot.exact_revision("@-").await.expect("parent");
        let comparison = Comparison::load(&snapshot, Comparison::new(&parent).intent)
            .await
            .expect("comparison");
        let file = comparison.files[0].clone();
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.activate_item(&ordinary, true, true, window, cx);
        });
        let single = cx
            .update(|window, cx| {
                KkDiff::open(
                    snapshot.clone(),
                    comparison.diff_target(),
                    file.clone(),
                    true,
                    workspace.downgrade(),
                    DiffOpenMode::Open,
                    window,
                    cx,
                )
            })
            .await
            .expect("single diff");
        let multiple = cx
            .update(|window, cx| {
                KkMultiDiff::open(
                    snapshot.clone(),
                    comparison.diff_target(),
                    comparison.files.clone(),
                    true,
                    workspace.downgrade(),
                    DiffOpenMode::Open,
                    window,
                    cx,
                )
            })
            .await
            .expect("aggregate diff");
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.activate_item(&ordinary, true, true, window, cx);
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.run_until_parked();
        let root = fixture.root.clone();
        cx.background_spawn(async move {
            run_kk(&root, &["describe", "-m", "changed outside Zed"]).await
        })
        .await
        .expect("external edit");
        workspace.update(cx, |_, cx| cx.emit(workspace::Event::TerminalFocusLost));
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.run_until_parked();
        let version = panel.read_with(cx, |panel, _| {
            assert_eq!(panel.refresh_sequence, list_requests);
            assert_eq!(panel.refresh_demand.list_version, list_version);
            panel.refresh_demand.version
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.activate_item(&single, true, true, window, cx);
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.run_until_parked();
        panel
            .update(cx, |panel, _| {
                std::mem::replace(&mut panel.refresh_demand.task, Task::ready(()))
            })
            .await;
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.refresh_demand.items.get(&single.entity_id()),
                Some(&version)
            );
            assert_ne!(
                panel.refresh_demand.items.get(&multiple.entity_id()),
                Some(&version)
            );
            assert_eq!(panel.refresh_demand.list_version, list_version);
            assert_eq!(
                panel.refresh_sequence, list_requests,
                "diff refresh must not load history"
            );
        });
        let refreshed = single.read_with(cx, |single, _| {
            single.snapshot().expect("snapshot").operation().to_owned()
        });
        assert_ne!(refreshed, snapshot.operation());
        assert_eq!(
            multiple.read_with(cx, |multiple, _| multiple.snapshot().operation().to_owned()),
            snapshot.operation()
        );
        assert!(cx.update(|window, cx| single.focus_handle(cx).contains_focused(window, cx)));
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.activate_item(&multiple, true, true, window, cx);
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.run_until_parked();
        panel
            .update(cx, |panel, _| {
                std::mem::replace(&mut panel.refresh_demand.task, Task::ready(()))
            })
            .await;
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.refresh_demand.items.get(&multiple.entity_id()),
                Some(&version)
            );
            assert_eq!(panel.refresh_demand.list_version, list_version);
            assert_eq!(panel.refresh_sequence, list_requests);
        });
        assert_eq!(
            multiple.read_with(cx, |multiple, _| multiple.snapshot().operation().to_owned()),
            refreshed
        );
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.focus_panel::<KkPanel>(window, cx);
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.run_until_parked();
        panel
            .update(cx, |panel, _| {
                std::mem::replace(&mut panel.refresh_task, Task::ready(()))
            })
            .await;
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.refresh_demand.list_version, version);
            assert_eq!(panel.refresh_sequence, list_requests + 1);
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.activate_item(&ordinary, true, true, window, cx);
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.focus_panel::<KkPanel>(window, cx);
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.refresh_sequence, list_requests + 1)
        });

        panel.update(cx, |panel, cx| {
            panel.invalidate_refresh(cx);
            panel.start_refresh_with_scope(false, cx);
            panel.invalidate_refresh(cx);
            assert!(panel.refresh_demand.running_version < panel.refresh_demand.version);
        });
        panel
            .update(cx, |panel, _| {
                std::mem::replace(&mut panel.refresh_task, Task::ready(()))
            })
            .await;
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.run_until_parked();
        panel
            .update(cx, |panel, _| {
                std::mem::replace(&mut panel.refresh_task, Task::ready(()))
            })
            .await;
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.refresh_demand.list_version,
                panel.refresh_demand.version
            )
        });

        let (blocked_list, pending_list) = async_channel::bounded::<()>(1);
        let previous_list_version =
            panel.read_with(cx, |panel, _| panel.refresh_demand.list_version);
        panel.update(cx, |panel, cx| {
            panel.invalidate_refresh(cx);
            panel.loading = true;
            panel.refresh_demand.refresh_open_diffs = false;
            panel.refresh_task = cx.spawn(async move |_, _| {
                pending_list
                    .recv()
                    .await
                    .expect("list must be cancelled before it can finish");
            });
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.activate_item(&single, true, true, window, cx);
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.run_until_parked();
        panel
            .update(cx, |panel, _| {
                std::mem::replace(&mut panel.refresh_demand.task, Task::ready(()))
            })
            .await;
        cx.run_until_parked();
        assert!(
            blocked_list.is_closed(),
            "a focused diff must not wait for inactive history loading"
        );
        panel.read_with(cx, |panel, _| {
            assert!(!panel.loading);
            assert_eq!(panel.refresh_demand.list_version, previous_list_version);
            assert_eq!(
                panel.refresh_demand.items.get(&single.entity_id()),
                Some(&panel.refresh_demand.version)
            );
        });

        workspace.update_in(cx, |workspace, window, cx| {
            workspace.activate_item(&ordinary, true, true, window, cx);
        });
        let root = fixture.root.clone();
        cx.background_spawn(async move {
            std::fs::rename(root.join(".kk"), root.join(".kk-unavailable"))
        })
        .await
        .expect("make temporary repository unavailable");
        workspace.update(cx, |_, cx| cx.emit(workspace::Event::TerminalFocusLost));
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.activate_item(&single, true, true, window, cx);
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.run_until_parked();
        panel
            .update(cx, |panel, _| {
                std::mem::replace(&mut panel.refresh_demand.task, Task::ready(()))
            })
            .await;
        panel.read_with(cx, |panel, _| {
            assert_ne!(
                panel.refresh_demand.items.get(&single.entity_id()),
                Some(&panel.refresh_demand.version)
            );
            assert!(!panel.refresh_demand.running);
        });
        let root = fixture.root.clone();
        cx.background_spawn(async move {
            std::fs::rename(root.join(".kk-unavailable"), root.join(".kk"))
        })
        .await
        .expect("restore temporary repository");
        let buffer = single.read_with(cx, |single, _| single.buffer.clone());
        buffer.update(cx, |buffer, cx| {
            buffer.edit([(0..0, "unsaved draft\n")], None, cx);
        });
        let draft = buffer.read_with(cx, |buffer, _| buffer.text());
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.activate_item(&ordinary, true, true, window, cx);
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.activate_item(&single, true, true, window, cx);
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.run_until_parked();
        panel
            .update(cx, |panel, _| {
                std::mem::replace(&mut panel.refresh_demand.task, Task::ready(()))
            })
            .await;
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.refresh_demand.items.get(&single.entity_id()),
                Some(&panel.refresh_demand.version)
            )
        });
        assert_eq!(buffer.read_with(cx, |buffer, _| buffer.text()), draft);
        assert!(buffer.read_with(cx, |buffer, _| buffer.is_dirty()));
    }
}
