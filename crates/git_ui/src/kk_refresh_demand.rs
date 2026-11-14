use super::*;
use gpui::AnyWindowHandle;

pub(super) struct RefreshDemand {
    pub version: u64,
    pub list_version: u64,
    window: AnyWindowHandle,
    queued: bool,
}

impl RefreshDemand {
    pub fn new(window: AnyWindowHandle) -> Self {
        Self {
            version: 1,
            list_version: 0,
            window,
            queued: false,
        }
    }

    pub fn invalidate(&mut self) {
        self.version += 1;
    }
}

impl KkPanel {
    pub(super) fn invalidate_refresh(&mut self, cx: &mut Context<Self>) {
        self.refresh_demand.invalidate();
        self.schedule_open_views_refresh(cx);
    }

    fn has_open_refresh_views(&self, window: &Window, cx: &Context<Self>) -> bool {
        let Some(workspace) = self.workspace.upgrade() else {
            return false;
        };
        let workspace = workspace.read(cx);
        let dock = workspace
            .dock_at_position(self.position(window, cx))
            .read(cx);
        dock.visible_panel()
            .is_some_and(|panel| panel.panel_id() == cx.entity_id())
            || workspace.items_of_type::<KkDiff>(cx).next().is_some()
            || workspace.items_of_type::<KkMultiDiff>(cx).next().is_some()
    }

    pub(super) fn schedule_open_views_refresh(&mut self, cx: &mut Context<Self>) {
        if self.loading || self.refresh_demand.queued {
            return;
        }

        self.refresh_demand.queued = true;
        let panel = cx.weak_entity();
        let window = self.refresh_demand.window;
        // Terminal and dock events can arrive while Workspace is being updated.
        cx.defer(move |cx| {
            window
                .update(cx, |_, window, cx| {
                    panel
                        .update(cx, |panel, cx| {
                            panel.refresh_demand.queued = false;
                            if window.is_window_active()
                                && panel.has_open_refresh_views(window, cx)
                                && !panel.loading
                                && panel.refresh_demand.list_version != panel.refresh_demand.version
                            {
                                panel.start_pending_refresh(cx);
                            }
                        })
                        .log_err();
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
    async fn kk_refresh_demand_updates_list_and_background_diffs(
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
            workspace.add_panel(panel.clone(), window, cx);
            window.activate_window();
            editor
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.refresh_sequence, 0,
                "closed views must not load history"
            );
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.open_panel::<KkPanel>(window, cx);
        });
        cx.run_until_parked();
        cx.condition(&panel, |panel, _| {
            !panel.loading && panel.refresh_demand.list_version == panel.refresh_demand.version
        })
        .await;
        let (snapshot, initial_requests) = panel.read_with(cx, |panel, _| {
            assert!(panel.error.is_none(), "{:?}", panel.error);
            (
                panel.repositories[0].snapshot.clone(),
                panel.refresh_sequence,
            )
        });
        workspace.update(cx, |_, cx| {
            cx.emit(workspace::Event::TerminalFocusLost);
            cx.emit(workspace::Event::TerminalFocusLost);
        });
        cx.run_until_parked();
        cx.condition(&panel, |panel, _| {
            !panel.loading && panel.refresh_demand.list_version == panel.refresh_demand.version
        })
        .await;
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.refresh_sequence, initial_requests + 1);
        });
        assert!(cx.update(|window, cx| ordinary.focus_handle(cx).contains_focused(window, cx)));
        let initial_requests = initial_requests + 1;
        let parent = snapshot.exact_revision("@-").await.expect("parent");
        let comparison = Comparison::load(&snapshot, Comparison::new(&parent).intent)
            .await
            .expect("comparison");
        let single = cx
            .update(|window, cx| {
                KkDiff::open(
                    snapshot.clone(),
                    comparison.diff_target(),
                    comparison.files[0].clone(),
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
        let buffer = single.read_with(cx, |single, _| single.buffer.clone());
        buffer.update(cx, |buffer, cx| {
            buffer.edit([(0..0, "unsaved draft\n")], None, cx);
        });
        let draft = buffer.read_with(cx, |buffer, _| buffer.text());
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.close_panel::<KkPanel>(window, cx);
        });
        let root = fixture.root.clone();
        cx.background_spawn(async move {
            run_kk(&root, &["describe", "-m", "changed outside Zed"]).await
        })
        .await
        .expect("external edit");
        workspace.update(cx, |_, cx| {
            cx.emit(workspace::Event::TerminalFocusLost);
            cx.emit(workspace::Event::TerminalFocusLost);
        });
        cx.run_until_parked();
        cx.condition(&panel, |panel, _| {
            !panel.loading && panel.refresh_demand.list_version == panel.refresh_demand.version
        })
        .await;
        let refreshed = panel.read_with(cx, |panel, _| {
            assert!(panel.error.is_none(), "{:?}", panel.error);
            assert_eq!(panel.refresh_sequence, initial_requests + 1);
            panel.repositories[0].snapshot.operation().to_owned()
        });
        assert_ne!(refreshed, snapshot.operation());
        single
            .update(cx, |single, _| single.pending_refresh())
            .await;
        multiple
            .update(cx, |multiple, _| multiple.pending_refresh())
            .await;
        single.read_with(cx, |single, _| {
            assert!(single.error().is_none(), "{:?}", single.error());
            assert_eq!(single.snapshot().expect("snapshot").operation(), refreshed);
        });
        multiple.read_with(cx, |multiple, _| {
            assert!(multiple.error().is_none(), "{:?}", multiple.error());
            assert_eq!(multiple.snapshot().operation(), refreshed);
        });
        assert!(cx.update(|window, cx| ordinary.focus_handle(cx).contains_focused(window, cx)));
        assert_eq!(buffer.read_with(cx, |buffer, _| buffer.text()), draft);
        assert!(buffer.read_with(cx, |buffer, _| buffer.is_dirty()));

        let previous_requests = panel.read_with(cx, |panel, _| panel.refresh_sequence);
        panel.update(cx, |panel, cx| {
            panel.invalidate_refresh(cx);
            panel.start_pending_refresh(cx);
            panel.invalidate_refresh(cx);
        });
        cx.run_until_parked();
        cx.condition(&panel, |panel, _| {
            !panel.loading && panel.refresh_demand.list_version == panel.refresh_demand.version
        })
        .await;
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.refresh_sequence, previous_requests + 2);
        });

        let root = fixture.root.clone();
        cx.background_spawn(async move {
            std::fs::rename(root.join(".kk/repo"), root.join(".kk/repo-unavailable"))
        })
        .await
        .expect("make temporary repository unavailable");
        workspace.update(cx, |_, cx| cx.emit(workspace::Event::TerminalFocusLost));
        cx.run_until_parked();
        cx.condition(&panel, |panel, _| !panel.loading && panel.error.is_some())
            .await;
        let failed_requests = panel.read_with(cx, |panel, _| {
            assert_ne!(
                panel.refresh_demand.list_version,
                panel.refresh_demand.version
            );
            panel.refresh_sequence
        });
        cx.run_until_parked();
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.refresh_sequence),
            failed_requests
        );
        let root = fixture.root.clone();
        cx.background_spawn(async move {
            std::fs::rename(root.join(".kk/repo-unavailable"), root.join(".kk/repo"))
        })
        .await
        .expect("restore temporary repository");
        workspace.update(cx, |_, cx| cx.emit(workspace::Event::TerminalFocusLost));
        cx.run_until_parked();
        cx.condition(&panel, |panel, _| {
            !panel.loading
                && panel.error.is_none()
                && panel.refresh_demand.list_version == panel.refresh_demand.version
        })
        .await;
        assert_eq!(buffer.read_with(cx, |buffer, _| buffer.text()), draft);

        let previous_requests = panel.read_with(cx, |panel, _| panel.refresh_sequence);
        cx.deactivate_window();
        let root = fixture.root.clone();
        cx.background_spawn(async move {
            run_kk(&root, &["describe", "-m", "changed while window inactive"]).await
        })
        .await
        .expect("external edit while inactive");
        workspace.update(cx, |_, cx| cx.emit(workspace::Event::TerminalFocusLost));
        cx.run_until_parked();
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.refresh_sequence),
            previous_requests
        );
        cx.update(|window, _| window.activate_window());
        cx.run_until_parked();
        cx.condition(&panel, |panel, _| {
            !panel.loading && panel.refresh_demand.list_version == panel.refresh_demand.version
        })
        .await;
        let activated = panel.read_with(cx, |panel, _| {
            assert_eq!(panel.refresh_sequence, previous_requests + 1);
            panel.repositories[0].snapshot.operation().to_owned()
        });
        assert_ne!(activated, refreshed);
        single
            .update(cx, |single, _| single.pending_refresh())
            .await;
        multiple
            .update(cx, |multiple, _| multiple.pending_refresh())
            .await;
        single.read_with(cx, |single, _| {
            assert!(single.error().is_none(), "{:?}", single.error());
            assert_eq!(single.snapshot().expect("snapshot").operation(), activated);
        });
        multiple.read_with(cx, |multiple, _| {
            assert!(multiple.error().is_none(), "{:?}", multiple.error());
            assert_eq!(multiple.snapshot().operation(), activated);
        });
        assert!(cx.update(|window, cx| ordinary.focus_handle(cx).contains_focused(window, cx)));
        assert_eq!(buffer.read_with(cx, |buffer, _| buffer.text()), draft);
    }
}
