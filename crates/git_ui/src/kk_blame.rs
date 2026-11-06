use super::kk_settings::KkWorkbenchSettings;
use anyhow::{Context as _, ensure};
use editor::{Addon, Editor, GutterAnnotations, SplittableEditor};
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, EntityId, SharedString, Subscription, Task,
    TextStyle, WeakEntity, Window, actions,
};
use image_viewer::ImageView;
use kk_workbench::{BlameLine, Diagnostics, Repository, Snapshot};
use language::{Buffer, BufferEvent, Capability};
use multi_buffer::{MultiBuffer, RowInfo};
use settings::{Settings, SettingsStore};
use std::{any::Any, collections::HashMap, path::PathBuf, sync::Arc};
use time::{OffsetDateTime, UtcOffset, format_description::well_known::Rfc3339};
use ui::prelude::*;
use util::ResultExt as _;
use workspace::{
    HideStatusItem, StatusItemView, Toast, Workspace, item::ItemHandle,
    notifications::NotificationId,
};

actions!(
    kk_workbench,
    [ToggleBlame, BlameFile, OpenFileAtBlamedRevision]
);

#[derive(Clone)]
pub(super) enum BlameTarget {
    WorkingFile {
        absolute: PathBuf,
        remote: Option<kk_workbench::remote::Client>,
    },
    Revision {
        snapshot: Snapshot,
        revision: String,
        path: String,
        refresh_buffer: bool,
        empty: bool,
    },
}

struct BlameState {
    target: BlameTarget,
    diagnostics: Diagnostics,
    buffer: WeakEntity<Buffer>,
    snapshot: Option<Snapshot>,
    path: String,
    enabled: bool,
    stale: bool,
    loading: bool,
    lines: Arc<Vec<BlameLine>>,
    annotated_snapshot: Option<language::BufferSnapshot>,
    error: Option<String>,
    generation: u64,
    task: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl BlameState {
    fn new(
        target: BlameTarget,
        buffer: &Entity<Buffer>,
        diagnostics: Diagnostics,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscriptions =
            vec![
                cx.subscribe(buffer, |this: &mut Self, buffer, event, cx| match event {
                    BufferEvent::Edited { .. } => {
                        this.stale = this.annotated_snapshot.as_ref().is_none_or(|snapshot| {
                            snapshot.version() != &buffer.read(cx).version()
                        });
                        cx.notify();
                        if this.stale
                            && this.enabled
                            && !buffer.read(cx).is_dirty()
                            && matches!(
                                this.target,
                                BlameTarget::Revision {
                                    refresh_buffer: false,
                                    ..
                                }
                            )
                        {
                            this.load(cx);
                        }
                    }
                    BufferEvent::Saved
                        if this.enabled
                            && matches!(this.target, BlameTarget::WorkingFile { .. }) =>
                    {
                        this.load(cx)
                    }
                    _ => {}
                }),
            ];
        Self {
            target,
            diagnostics,
            buffer: buffer.downgrade(),
            snapshot: None,
            path: String::new(),
            enabled: false,
            stale: false,
            loading: false,
            lines: Arc::new(Vec::new()),
            annotated_snapshot: None,
            error: None,
            generation: 0,
            task: Task::ready(()),
            _subscriptions: subscriptions,
        }
    }

    fn disable(&mut self, cx: &mut Context<Self>) {
        self.enabled = false;
        self.generation += 1;
        self.task = Task::ready(());
        self.loading = false;
        self.error = None;
        cx.notify();
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(buffer) = self.buffer.upgrade() else {
            return;
        };
        if buffer.read(cx).is_dirty() {
            self.stale = true;
            cx.notify();
            return;
        }

        let version = buffer.read(cx).version();
        self.generation += 1;
        let generation = self.generation;
        let target = self.target.clone();
        let enabled = self.enabled;
        let executable = KkWorkbenchSettings::get_global(cx).executable_path.clone();
        let diagnostics = self.diagnostics.clone();
        self.loading = true;
        self.error = None;
        self.task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let (snapshot, revision, path, historical, empty) = match target {
                        BlameTarget::WorkingFile { absolute, remote } => {
                            let directory =
                                absolute.parent().context("File has no parent directory")?;
                            let repository = if let Some(remote) = remote {
                                remote.open(directory, &executable, diagnostics).await?
                            } else {
                                Repository::open_with_diagnostics(
                                    directory,
                                    &executable,
                                    diagnostics,
                                    None,
                                )
                                .await?
                            };
                            let path = repository.relative_file_path(&absolute).await?;
                            (
                                repository.snapshot(true).await?,
                                "@".to_owned(),
                                path,
                                false,
                                false,
                            )
                        }
                        BlameTarget::Revision {
                            snapshot,
                            revision,
                            path,
                            refresh_buffer,
                            empty,
                        } => (snapshot, revision, path, refresh_buffer, empty),
                    };
                    let bytes = if empty {
                        Vec::new()
                    } else {
                        snapshot.file_content(&revision, &path).await?
                    };
                    ensure!(!bytes.contains(&0), "Blame is unavailable for binary files");
                    let mut text = String::from_utf8(bytes).context("File is not UTF-8")?;
                    if text.starts_with('\u{feff}') {
                        text.remove(0);
                    }

                    language::LineEnding::normalize(&mut text);
                    let lines = if enabled && !empty {
                        snapshot.annotate(&revision, &path).await?
                    } else {
                        Vec::new()
                    };
                    anyhow::Ok((snapshot, path, historical, text, lines))
                })
                .await;
            let result = async {
                let (snapshot, path, historical, text, lines) = result?;
                let edits = if historical {
                    Some(
                        buffer
                            .read_with(cx, |buffer, cx| buffer.diff(text.clone(), cx))
                            .await,
                    )
                } else {
                    None
                };
                this.update(cx, |this, cx| {
                    if generation != this.generation {
                        return;
                    }

                    this.loading = false;
                    if buffer.read(cx).version() != version || buffer.read(cx).is_dirty() {
                        this.stale = true;
                        cx.notify();
                        return;
                    }

                    if let Some(edits) = edits {
                        buffer.update(cx, |buffer, cx| {
                            buffer.apply_diff(edits, cx);
                            buffer.did_save(buffer.version(), None, cx);
                        });
                    }

                    this.stale = buffer.read(cx).text() != text;
                    this.annotated_snapshot = Some(buffer.read(cx).snapshot());
                    this.snapshot = Some(snapshot);
                    this.path = path;
                    this.lines = Arc::new(lines);
                    this.error = None;
                    cx.notify();
                })?;
                anyhow::Ok(())
            }
            .await;
            if let Err(error) = result {
                this.update(cx, |this, cx| {
                    if generation == this.generation {
                        this.loading = false;
                        this.stale = true;
                        this.error = Some(format!("Could not load blame: {error:#}"));
                        cx.notify();
                    }
                })
                .log_err();
            }
        });
        cx.notify();
    }

    fn refresh(&mut self, snapshot: &Snapshot, cx: &mut Context<Self>) {
        if self.snapshot.as_ref().is_some_and(|previous| {
            previous.repository().root() == snapshot.repository().root()
                && previous.operation() != snapshot.operation()
        }) {
            if let BlameTarget::Revision {
                snapshot: previous, ..
            } = &mut self.target
            {
                *previous = snapshot.clone();
            }

            if self.enabled || matches!(self.target, BlameTarget::Revision { .. }) {
                self.load(cx);
            }
        }
    }
}

struct BlameAddon {
    state: Entity<BlameState>,
    _subscription: Subscription,
}

impl Addon for BlameAddon {
    fn gutter_annotations(&self, cx: &App) -> Option<Arc<dyn GutterAnnotations>> {
        let state = self.state.read(cx);
        if !state.enabled || state.stale || state.lines.is_empty() {
            return None;
        }

        let buffer = state.buffer.upgrade()?;
        Some(Arc::new(BlameColumn {
            buffer_id: buffer.read(cx).remote_id(),
            lines: state.lines.clone(),
            author_width: state
                .lines
                .iter()
                .map(|line| line.author.chars().count())
                .max()
                .unwrap_or(4)
                .clamp(4, 20),
        }))
    }

    fn to_any(&self) -> &dyn Any {
        self
    }
}

struct BlameColumn {
    buffer_id: language::BufferId,
    lines: Arc<Vec<BlameLine>>,
    author_width: usize,
}

impl GutterAnnotations for BlameColumn {
    fn width_in_characters(&self, cx: &App) -> usize {
        KkWorkbenchSettings::get_global(cx).blame.column_width as usize + 2
    }

    fn render_row(
        &self,
        row: &RowInfo,
        style: &TextStyle,
        _: &mut Window,
        cx: &mut App,
    ) -> Option<AnyElement> {
        if row.buffer_id != Some(self.buffer_id) {
            return None;
        }

        let line = self.lines.get(row.buffer_row? as usize)?;
        let settings = &KkWorkbenchSettings::get_global(cx).blame;
        let label = column_label(
            line,
            settings.column_width as usize,
            settings.compact,
            self.author_width,
        );
        let tooltip = vec![
            ("Author".to_owned(), line.author.clone(), Color::Info),
            ("Date".to_owned(), relative_date(&line.date), Color::Muted),
            (
                "Change ID".to_owned(),
                line.change_id.clone(),
                Color::Accent,
            ),
            (
                String::new(),
                if line.summary.is_empty() {
                    "No description".to_owned()
                } else {
                    line.summary.clone()
                },
                Color::Default,
            ),
        ];
        let identifier_end = format!("{:<8} ", line.change_id).len();
        let author_end =
            identifier_end + format!("{:<width$} ", line.author, width = self.author_width).len();
        let highlights = if settings.compact && !line.first {
            Vec::new()
        } else {
            [
                (
                    0..line.change_id_prefix.len().min(line.change_id.len()),
                    Color::Accent,
                ),
                (identifier_end..author_end, Color::Info),
            ]
            .into_iter()
            .filter_map(|(range, color)| {
                let start = label.floor_char_boundary(range.start.min(label.len()));
                let end = label.floor_char_boundary(range.end.min(label.len()));
                (start < end).then_some((
                    start..end,
                    gpui::HighlightStyle {
                        color: Some(color.color(cx)),
                        ..Default::default()
                    },
                ))
            })
            .collect::<Vec<_>>()
        };
        Some(
            div()
                .id(("kk-blame", row.buffer_row? as usize))
                .h_full()
                .overflow_hidden()
                .whitespace_nowrap()
                .font(style.font())
                .text_size(style.font_size)
                .text_color(cx.theme().colors().text_muted)
                .child(gpui::StyledText::new(label).with_highlights(highlights))
                .tooltip(super::commit_metadata::tooltip_with_prefix(
                    tooltip,
                    Some(line.change_id_prefix.clone()),
                ))
                .into_any_element(),
        )
    }
}

fn column_label(line: &BlameLine, width: usize, compact: bool, author_width: usize) -> String {
    let label = if compact && !line.first {
        "│".to_owned()
    } else {
        let date = OffsetDateTime::parse(&line.date, &Rfc3339)
            .ok()
            .map(|date| {
                let date =
                    date.to_offset(UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC));
                format!(
                    "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                    date.year(),
                    u8::from(date.month()),
                    date.day(),
                    date.hour(),
                    date.minute(),
                    date.second()
                )
            })
            .unwrap_or_else(|| {
                line.date
                    .chars()
                    .take(19)
                    .collect::<String>()
                    .replace('T', " ")
            });
        format!(
            "{:<8} {:<author_width$} {date}",
            line.change_id, line.author
        )
    };
    if label.chars().count() > width {
        format!(
            "{}…",
            label
                .chars()
                .take(width.saturating_sub(1))
                .collect::<String>()
        )
    } else {
        format!("{label}{}", "\u{a0}".repeat(width - label.chars().count()))
    }
}

fn relative_date(value: &str) -> String {
    let Some(elapsed) = OffsetDateTime::parse(value, &Rfc3339)
        .ok()
        .map(|date| (OffsetDateTime::now_utc() - date).whole_seconds())
        .filter(|elapsed| *elapsed >= 0)
    else {
        return value.chars().take(10).collect();
    };
    for (seconds, unit) in [
        (365 * 86400, "y"),
        (30 * 86400, "mo"),
        (86400, "d"),
        (3600, "h"),
        (60, "m"),
    ] {
        if elapsed >= seconds {
            return format!("{}{unit} ago", elapsed / seconds);
        }
    }

    "just now".into()
}

pub struct KkBlameStatus {
    workspace: WeakEntity<Workspace>,
    active_editor: Option<Entity<Editor>>,
    active_split: Option<Entity<SplittableEditor>>,
    states: HashMap<EntityId, WeakEntity<BlameState>>,
    images: HashMap<HistoricalImageKey, HistoricalImage>,
    open_task: Task<()>,
    _active_subscriptions: Vec<Subscription>,
    _settings_subscription: Subscription,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct HistoricalImageKey {
    root: PathBuf,
    revision: String,
    path: String,
}

struct HistoricalImage {
    operation: Option<String>,
    source: EntityId,
    view: WeakEntity<ImageView>,
    task: Task<()>,
}

enum HistoricalFileContent {
    Text(String),
    Image(Arc<gpui::Image>, project::image_store::ImageMetadata),
}

impl HistoricalFileContent {
    async fn load(
        snapshot: &Snapshot,
        revision: &str,
        path: &str,
        empty: bool,
    ) -> anyhow::Result<Self> {
        let mut bytes = if empty {
            Vec::new()
        } else {
            snapshot.file_content(revision, path).await?
        };
        if let Some(pointer) = kk_workbench::LfsPointer::parse(&bytes) {
            bytes = snapshot.repository().lfs_content(&pointer).await?;
        }

        let image = !empty
            && std::path::Path::new(path)
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| {
                    let extension = extension.to_lowercase();
                    extension != "svg" && gpui::Img::extensions().contains(&extension.as_str())
                });
        if image {
            let metadata = project::ImageItem::compute_metadata_from_bytes(&bytes)?;
            return Ok(Self::Image(
                Arc::new(gpui::Image::from_bytes(
                    Self::image_format(metadata.format)?,
                    bytes,
                )),
                metadata,
            ));
        }

        ensure!(
            !bytes.contains(&0),
            "Cannot display this binary file as text"
        );
        let mut text = String::from_utf8(bytes)?;
        if text.starts_with('\u{feff}') {
            text.remove(0);
        }

        Ok(Self::Text(text))
    }

    fn image_format(
        format: project::image_store::ImageFormat,
    ) -> anyhow::Result<gpui::ImageFormat> {
        use project::image_store::{ImageFormat, RasterImageFormat};

        Ok(match format {
            ImageFormat::Svg => gpui::ImageFormat::Svg,
            ImageFormat::Raster(format) => match format {
                RasterImageFormat::Png => gpui::ImageFormat::Png,
                RasterImageFormat::Jpeg => gpui::ImageFormat::Jpeg,
                RasterImageFormat::WebP => gpui::ImageFormat::Webp,
                RasterImageFormat::Gif => gpui::ImageFormat::Gif,
                RasterImageFormat::Bmp => gpui::ImageFormat::Bmp,
                RasterImageFormat::Tiff => gpui::ImageFormat::Tiff,
                RasterImageFormat::Ico => gpui::ImageFormat::Ico,
                RasterImageFormat::Pnm => gpui::ImageFormat::Pnm,
                format => anyhow::bail!("Image format {format:?} not supported"),
            },
        })
    }
}

impl KkBlameStatus {
    pub fn new(workspace: &Workspace, cx: &mut Context<Self>) -> Self {
        Self {
            workspace: workspace.weak_handle(),
            active_editor: None,
            active_split: None,
            states: HashMap::new(),
            images: HashMap::new(),
            open_task: Task::ready(()),
            _active_subscriptions: Vec::new(),
            _settings_subscription: cx.observe_global::<SettingsStore>(|_, cx| cx.notify()),
        }
    }

    fn editor(&self, cx: &App) -> Option<Entity<Editor>> {
        self.active_split
            .as_ref()
            .map(|split| split.read(cx).focused_editor().clone())
            .or_else(|| self.active_editor.clone())
    }

    fn attach(
        &mut self,
        editor: &Entity<Editor>,
        buffer: &Entity<Buffer>,
        target: BlameTarget,
        cx: &mut Context<Self>,
    ) -> Entity<BlameState> {
        let state = self
            .states
            .get(&buffer.entity_id())
            .and_then(WeakEntity::upgrade)
            .unwrap_or_else(|| {
                let diagnostics = self
                    .workspace
                    .upgrade()
                    .and_then(|workspace| workspace.read(cx).panel::<super::kk_panel::KkPanel>(cx))
                    .map(|panel| panel.read(cx).diagnostics(cx))
                    .unwrap_or_else(|| match &target {
                        BlameTarget::Revision { snapshot, .. } => {
                            snapshot.repository().diagnostics().clone()
                        }
                        BlameTarget::WorkingFile { .. } => Diagnostics::default(),
                    });
                let state = cx.new(|cx| BlameState::new(target, buffer, diagnostics, cx));
                self.states.insert(buffer.entity_id(), state.downgrade());
                state
            });
        editor.update(cx, |editor, cx| {
            let workspace = self.workspace.clone();
            let mut previous_error = None;
            let subscription = cx.observe(&state, move |_, state, cx| {
                let error = state.read(cx).error.clone();
                if error != previous_error
                    && let Some(error) = error.as_ref()
                    && let Some(workspace) = workspace.upgrade()
                {
                    let error = error.clone();
                    cx.defer(move |cx| {
                        workspace.update(cx, |workspace, cx| {
                            workspace.show_toast(
                                Toast::new(NotificationId::unique::<BlameState>(), error),
                                cx,
                            )
                        })
                    });
                }

                previous_error = error;
                cx.notify();
            });
            editor.register_addon(BlameAddon {
                state: state.clone(),
                _subscription: subscription,
            });
            cx.notify();
        });
        state
    }

    fn error(&self, message: String, cx: &mut Context<Self>) {
        if let Some(workspace) = self.workspace.upgrade() {
            cx.defer(move |cx| {
                workspace.update(cx, |workspace, cx| {
                    workspace.show_toast(Toast::new(NotificationId::unique::<Self>(), message), cx)
                })
            });
        }
    }

    fn toggle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.editor(cx) else {
            return;
        };
        if let Some(workspace) = self.workspace.upgrade()
            && let Some(diff) = workspace
                .read(cx)
                .active_item_as::<super::kk_diff::KkDiff>(cx)
        {
            let targets = diff.read(cx).blame_targets(cx);
            for (editor, buffer, target) in targets {
                if editor.read(cx).addon::<BlameAddon>().is_none() {
                    self.attach(&editor, &buffer, target, cx);
                }
            }
        }

        let existing = editor
            .read(cx)
            .addon::<BlameAddon>()
            .map(|addon| addon.state.clone());
        if let Some(state) = existing.as_ref().filter(|state| state.read(cx).enabled) {
            for state in self.related_states(state, cx) {
                state.update(cx, |state, cx| state.disable(cx));
            }

            return;
        }

        let Some(buffer) = editor.read(cx).active_buffer(cx) else {
            return;
        };
        if buffer.read(cx).is_dirty() {
            self.error(
                "Save the file before enabling blame so annotations match the current lines."
                    .into(),
                cx,
            );
            return;
        }

        let state = if let Some(state) = existing {
            state
        } else {
            let Some(path) = buffer
                .read(cx)
                .file()
                .and_then(|file| file.file_system_abs_path(cx))
            else {
                self.error("This editor does not identify a KK revision.".into(), cx);
                return;
            };
            let remote = self.workspace.upgrade().and_then(|workspace| {
                workspace
                    .read(cx)
                    .project()
                    .read(cx)
                    .kk_workbench_client(cx)
            });
            if remote.is_none() && buffer.read(cx).file().is_some_and(|file| !file.is_local()) {
                self.error(
                    "KK blame requires a remote host connection for this file.".into(),
                    cx,
                );
                return;
            }

            self.attach(
                &editor,
                &buffer,
                BlameTarget::WorkingFile {
                    absolute: path,
                    remote,
                },
                cx,
            )
        };
        self.attach_related_editors(cx);
        for state in self.related_states(&state, cx) {
            state.update(cx, |state, cx| {
                state.enabled = true;
                state.load(cx);
            });
        }

        self.set_editor_subscriptions(window, cx);
    }

    fn related_states(&self, state: &Entity<BlameState>, cx: &App) -> Vec<Entity<BlameState>> {
        let BlameTarget::Revision { snapshot, path, .. } = &state.read(cx).target else {
            return vec![state.clone()];
        };
        self.states.values().filter_map(WeakEntity::upgrade).filter(|candidate| matches!(&candidate.read(cx).target, BlameTarget::Revision { snapshot: other, path: other_path, .. } if other.repository().root() == snapshot.repository().root() && other_path == path)).collect()
    }

    fn attach_related_editors(&mut self, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let editors = workspace
            .read(cx)
            .items(cx)
            .flat_map(|item| {
                if let Some(split) = item.act_as::<SplittableEditor>(cx) {
                    let split = split.read(cx);
                    std::iter::once(split.rhs_editor().clone())
                        .chain(split.lhs_editor().cloned())
                        .collect::<Vec<_>>()
                } else {
                    item.act_as::<Editor>(cx).into_iter().collect()
                }
            })
            .collect::<Vec<_>>();
        for editor in editors {
            if editor.read(cx).addon::<BlameAddon>().is_some() {
                continue;
            }

            if let Some(buffer) = editor.read(cx).active_buffer(cx)
                && let Some(state) = self
                    .states
                    .get(&buffer.entity_id())
                    .and_then(WeakEntity::upgrade)
            {
                let target = state.read(cx).target.clone();
                self.attach(&editor, &buffer, target, cx);
            }
        }
    }

    fn set_editor_subscriptions(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self._active_subscriptions.clear();
        if let Some(split) = self.active_split.clone() {
            self._active_subscriptions
                .push(cx.observe_in(&split, window, |_, _, _, cx| cx.notify()));
        }

        let editors = self
            .active_split
            .as_ref()
            .map(|split| {
                let split = split.read(cx);
                std::iter::once(split.rhs_editor().clone())
                    .chain(split.lhs_editor().cloned())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_else(|| self.active_editor.iter().cloned().collect());
        for editor in editors {
            self._active_subscriptions
                .push(cx.observe_in(&editor, window, |_, _, _, cx| cx.notify()));
        }

        cx.notify();
    }

    fn current_line(&self, cx: &App) -> Option<(Entity<BlameState>, BlameLine)> {
        let editor = self.editor(cx)?;
        let editor = editor.read(cx);
        let state = editor.addon::<BlameAddon>()?.state.clone();
        let data = state.read(cx);
        if !data.enabled || data.stale || data.loading || data.error.is_some() {
            return None;
        }

        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let (anchor, buffer) =
            snapshot.anchor_to_buffer_anchor(editor.selections.newest_anchor().head())?;
        let point = buffer.summary_for_anchor::<language::Point>(&anchor);
        let line = data.lines.get(point.row as usize)?.clone();
        Some((state, line))
    }

    fn open_blamed_revision(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((state, line)) = self.current_line(cx) else {
            self.error(
                "Save the file and load blame before opening a blamed revision.".into(),
                cx,
            );
            return;
        };
        let state = state.read(cx);
        let Some(snapshot) = state.snapshot.clone() else {
            return;
        };
        self.open_file(
            snapshot,
            format!("change_id({})", line.full_change_id),
            state.path.clone(),
            false,
            window,
            cx,
        );
    }

    fn retain_image_views(&mut self, cx: &App) {
        let views = self
            .workspace
            .upgrade()
            .map(|workspace| {
                workspace
                    .read(cx)
                    .items_of_type::<ImageView>(cx)
                    .map(|view| (view.read(cx).source_entity_id(), view.downgrade()))
                    .collect::<HashMap<_, _>>()
            })
            .unwrap_or_default();
        self.images.retain(|_, image| {
            if let Some(view) = views.get(&image.source) {
                image.view = view.clone();
                true
            } else {
                false
            }
        });
    }

    pub(super) fn refresh(&mut self, snapshot: &Snapshot, cx: &mut Context<Self>) {
        self.states.retain(|_, state| state.is_upgradable());
        for state in self.states.values().filter_map(WeakEntity::upgrade) {
            state.update(cx, |state, cx| state.refresh(snapshot, cx));
        }

        self.retain_image_views(cx);
        for (key, image) in &mut self.images {
            if key.root != snapshot.repository().root()
                || image.operation.as_deref() == Some(snapshot.operation())
            {
                continue;
            }

            image.operation = Some(snapshot.operation().to_owned());
            let operation = snapshot.operation().to_owned();
            let key = key.clone();
            let snapshot = snapshot.clone();
            image.task = cx.spawn(async move |this, cx| {
                let loaded = cx
                    .background_spawn({
                        let key = key.clone();
                        async move {
                            HistoricalFileContent::load(&snapshot, &key.revision, &key.path, false)
                                .await
                        }
                    })
                    .await;
                this.update(cx, |this, cx| {
                    this.retain_image_views(cx);
                    let view = this
                        .images
                        .get(&key)
                        .filter(|image| image.operation.as_deref() == Some(&operation))
                        .and_then(|image| image.view.upgrade());
                    let Some(view) = view else {
                        return;
                    };
                    let result = match loaded {
                        Ok(HistoricalFileContent::Image(image, metadata)) => {
                            view.update(cx, |view, cx| view.set_image(image, metadata, cx))
                        }
                        Ok(HistoricalFileContent::Text(_)) => Err(anyhow::anyhow!(
                            "The historical image is no longer an image"
                        )),
                        Err(error) => Err(error),
                    };
                    if let Err(error) = result {
                        if let Some(image) = this.images.get_mut(&key) {
                            image.operation = None;
                        }

                        this.error(format!("Could not refresh {}: {error:#}", key.path), cx);
                    }
                })
                .log_err();
            });
        }
    }

    pub(super) fn open_file(
        &mut self,
        snapshot: Snapshot,
        revision: String,
        path: String,
        empty: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let task = self.open_file_task(snapshot, revision, path, empty, window, cx);
        self.open_task = cx.spawn(async move |this, cx| {
            if let Err(error) = task.await {
                this.update(cx, |this, cx| {
                    this.error(format!("Could not open historical file: {error:#}"), cx)
                })
                .log_err();
            }
        });
    }

    pub(super) fn open_file_task(
        &mut self,
        snapshot: Snapshot,
        revision: String,
        path: String,
        empty: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        let Some(workspace) = self.workspace.upgrade() else {
            return Task::ready(Err(anyhow::anyhow!("Workspace was closed")));
        };
        let image_key = HistoricalImageKey {
            root: snapshot.repository().root().to_owned(),
            revision: revision.clone(),
            path: path.clone(),
        };
        self.retain_image_views(cx);
        if !empty
            && let Some(view) = self
                .images
                .get(&image_key)
                .and_then(|image| image.view.upgrade())
        {
            self.refresh(&snapshot, cx);
            workspace.update(cx, |workspace, cx| {
                workspace.activate_item(&view, true, true, window, cx)
            });
            return Task::ready(Ok(()));
        }

        let existing = workspace.read(cx).items_of_type::<Editor>(cx).find(|editor| {
            editor.read(cx).addon::<BlameAddon>().is_some_and(|addon| matches!(&addon.state.read(cx).target,
                BlameTarget::Revision { snapshot: previous, revision: previous_revision, path: previous_path, refresh_buffer: true, empty: previous_empty }
                if previous.repository().root() == snapshot.repository().root() && previous_revision == &revision && previous_path == &path && previous_empty == &empty))
        });
        if let Some(editor) = existing {
            if let Some(state) = editor
                .read(cx)
                .addon::<BlameAddon>()
                .map(|addon| addon.state.clone())
            {
                state.update(cx, |state, cx| state.refresh(&snapshot, cx));
            }

            workspace.update(cx, |workspace, cx| {
                workspace.activate_item(&editor, true, true, window, cx);
            });
            return Task::ready(Ok(()));
        }

        let project = workspace.read(cx).project().clone();
        let target = BlameTarget::Revision {
            snapshot: snapshot.clone(),
            revision: revision.clone(),
            path: path.clone(),
            refresh_buffer: true,
            empty,
        };
        cx.spawn_in(window, async move |this, cx| {
            let operation = snapshot.operation().to_owned();
            let content = cx
                .background_spawn({
                    let path = path.clone();
                    async move { HistoricalFileContent::load(&snapshot, &revision, &path, empty).await }
                })
                .await?;
            let text = match content {
                HistoricalFileContent::Text(text) => (text, path),
                HistoricalFileContent::Image(image, metadata) => {
                    let title = SharedString::from(format!("{} ({})", path, image_key.revision));
                    let view = workspace.update_in(cx, |workspace, window, cx| {
                        let view = cx.new(|cx| ImageView::from_image(image, title, metadata, project, window, cx));
                        workspace.add_item_to_active_pane(Box::new(view.clone()), None, true, window, cx);
                        view
                    })?;
                    this.update(cx, |this, cx| {
                        this.images.insert(image_key, HistoricalImage { operation: Some(operation), source: view.read(cx).source_entity_id(), view: view.downgrade(), task: Task::ready(()) });
                    })?;
                    return Ok(());
                }
            };
            let registry = project.read_with(cx, |project, _| project.languages().clone());
            let worktree_id = project.read_with(cx, |project, cx| {
                project.find_project_path(&image_key.root.join(&text.1), cx).map(|path| path.worktree_id)
                    .or_else(|| project.worktrees(cx).next().map(|worktree| worktree.read(cx).id()))
                    .unwrap_or_else(|| project::WorktreeId::from_usize(0))
            });
            let blob = Arc::new(crate::commit_view::GitBlob {
                path: git::repository::RepoPath::new(&text.1)?,
                worktree_id,
                is_deleted: empty,
                is_binary: false,
                display_name: text.1.clone(),
            });
            let buffer = crate::commit_view::build_buffer(text.0, blob, &registry, cx).await?;
            buffer.update(cx, |buffer, cx| {
                buffer.set_capability(Capability::ReadOnly, cx);
            });
            let editor = workspace.update_in(cx, |workspace, window, cx| {
                let multibuffer =
                    cx.new(|cx| MultiBuffer::singleton(buffer.clone(), cx).with_title(text.1));
                let editor =
                    cx.new(|cx| Editor::for_multibuffer(multibuffer, Some(project), window, cx));
                workspace.add_item_to_active_pane(Box::new(editor.clone()), None, true, window, cx);
                editor
            })?;
            this.update(cx, |this, cx| {
                let state = this.attach(&editor, &buffer, target, cx);
                state.update(cx, |state, cx| state.load(cx));
            })?;
            anyhow::Ok(())
        })
    }
}

impl Render for KkBlameStatus {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut text: Option<SharedString> = None;
        let mut tooltip = String::new();
        let mut metadata = None;
        let mut change_id_prefix = None;
        if KkWorkbenchSettings::get_global(cx).blame.show_status_bar
            && let Some(editor) = self.editor(cx)
            && let Some(addon) = editor.read(cx).addon::<BlameAddon>()
        {
            let state = addon.state.read(cx);
            if state.enabled {
                if let Some(error) = &state.error {
                    text = Some("Blame unavailable".into());
                    tooltip = error.clone();
                } else if state.loading && state.lines.is_empty() {
                    text = Some("Loading blame…".into());
                } else if state.stale {
                    text = Some("Save to refresh blame".into());
                    tooltip = "Blame annotations are hidden until the file is saved.".into();
                } else if let Some((_, line)) = self.current_line(cx) {
                    change_id_prefix = Some(line.change_id_prefix.clone());
                    text = Some(format!("{}, {}", line.author, relative_date(&line.date)).into());
                    metadata = Some(vec![
                        (
                            "Change ID".to_owned(),
                            line.change_id.clone(),
                            Color::Accent,
                        ),
                        ("Author".to_owned(), line.author.clone(), Color::Info),
                        ("Date".to_owned(), relative_date(&line.date), Color::Muted),
                        (
                            String::new(),
                            if line.summary.is_empty() {
                                "No description".to_owned()
                            } else {
                                line.summary.clone()
                            },
                            Color::Default,
                        ),
                    ]);
                    tooltip = format!(
                        "{} ({})",
                        if line.summary.is_empty() {
                            "No description"
                        } else {
                            &line.summary
                        },
                        line.change_id
                    );
                }
            }
        }

        div().when_some(text, |view, text| {
            view.child(
                ui::ButtonLike::new("kk-blame-status")
                    .child(
                        h_flex()
                            .gap_1()
                            .child(Icon::new(IconName::GitCommit).size(IconSize::Small))
                            .when_some(metadata.as_ref(), |view, fields| {
                                view.child(
                                    super::commit_metadata::person(
                                        fields
                                            .iter()
                                            .find(|(title, _, _)| title == "Author")
                                            .map(|(_, value, _)| value.clone())
                                            .unwrap_or_default(),
                                    )
                                    .size(LabelSize::Small),
                                )
                                .child(Label::new(",").color(Color::Muted).size(LabelSize::Small))
                                .child(
                                    Label::new(
                                        fields
                                            .iter()
                                            .find(|(title, _, _)| title == "Date")
                                            .map(|(_, value, _)| value.clone())
                                            .unwrap_or_default(),
                                    )
                                    .color(Color::Muted)
                                    .size(LabelSize::Small),
                                )
                            })
                            .when(metadata.is_none(), |view| {
                                view.child(
                                    Label::new(text).size(LabelSize::Small).color(Color::Muted),
                                )
                            }),
                    )
                    .tooltip(super::commit_metadata::tooltip_with_prefix(
                        metadata.unwrap_or_else(|| vec![(String::new(), tooltip, Color::Muted)]),
                        change_id_prefix,
                    ))
                    .on_click(cx.listener(|this, _, window, cx| this.toggle(window, cx))),
            )
        })
    }
}

impl StatusItemView for KkBlameStatus {
    fn set_active_pane_item(
        &mut self,
        item: Option<&dyn ItemHandle>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.active_split = item.and_then(|item| item.act_as::<SplittableEditor>(cx));
        self.active_editor = item.and_then(|item| item.act_as::<Editor>(cx));
        if let Some(editor) = self.editor(cx)
            && editor.read(cx).addon::<BlameAddon>().is_none()
            && let Some(buffer) = editor.read(cx).active_buffer(cx)
            && let Some(state) = self
                .states
                .get(&buffer.entity_id())
                .and_then(WeakEntity::upgrade)
        {
            let target = state.read(cx).target.clone();
            self.attach(&editor, &buffer, target, cx);
        }

        self.set_editor_subscriptions(window, cx);
    }

    fn hide_setting(&self, _: &App) -> Option<HideStatusItem> {
        Some(HideStatusItem::new(|settings| {
            settings
                .kk_workbench
                .get_or_insert_default()
                .blame
                .get_or_insert_default()
                .show_status_bar = Some(false)
        }))
    }
}

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleBlame, window, cx| {
            if let Some(status) = workspace
                .status_bar()
                .read(cx)
                .item_of_type::<KkBlameStatus>()
            {
                window.defer(cx, move |window, cx| {
                    status.update(cx, |status, cx| status.toggle(window, cx))
                });
            }
        });
        workspace.register_action(|workspace, _: &BlameFile, window, cx| {
            if let Some(status) = workspace
                .status_bar()
                .read(cx)
                .item_of_type::<KkBlameStatus>()
            {
                window.defer(cx, move |window, cx| {
                    status.update(cx, |status, cx| status.toggle(window, cx))
                });
            }
        });
        workspace.register_action(|workspace, _: &OpenFileAtBlamedRevision, window, cx| {
            if let Some(status) = workspace
                .status_bar()
                .read(cx)
                .item_of_type::<KkBlameStatus>()
            {
                window.defer(cx, move |window, cx| {
                    status.update(cx, |status, cx| status.open_blamed_revision(window, cx))
                });
            }
        });
    })
    .detach();
}

#[cfg(test)]
mod tests {
    #[test]
    fn kk_blame_column_preserves_unicode_width_and_compact_padding() {
        let mut line = kk_workbench::BlameLine {
            change_id: "abcdefgh".into(),
            change_id_prefix: "abc".into(),
            full_change_id: "abcdefgh".into(),
            commit_id: "12345678".into(),
            author: "张三🙂Long author".into(),
            date: "unknown date".into(),
            summary: "description".into(),
            first: true,
        };
        for width in [24, 42, 80] {
            let label = super::column_label(&line, width, false, 18);
            assert_eq!(label.chars().count(), width);
            assert!(label.starts_with("abcdefgh "));
            line.first = false;
            let compact = super::column_label(&line, width, true, 18);
            assert_eq!(compact.chars().count(), width);
            assert_eq!(compact.trim_end_matches('\u{a0}'), "│");
        }
    }

    use super::*;
    use crate::kk_panel::remote_tests::run_kk;
    use gpui::{TestAppContext, VisualContext};
    use project::{FakeFs, Project};
    use std::path::Path;
    use workspace::MultiWorkspace;

    #[gpui::test(iterations = 3)]
    async fn kk_remote_blame_loads_and_refreshes_saved_versions(
        cx: &mut TestAppContext,
        server_cx: &mut TestAppContext,
    ) {
        run_remote_blame(cx, server_cx).await.expect("remote blame");
    }

    async fn run_remote_blame(
        cx: &mut TestAppContext,
        server_cx: &mut TestAppContext,
    ) -> anyhow::Result<()> {
        let fixture = crate::kk_panel::remote_tests::RemoteFixture::create(cx, server_cx).await?;
        let project = fixture.project.clone();
        let absolute = fixture.root.join("file.txt");
        let path = project
            .read_with(cx, |project, cx| project.find_project_path(&absolute, cx))
            .context("remote project path")?;
        let buffer = project
            .update(cx, |project, cx| project.open_buffer(path, cx))
            .await?;
        let remote = project
            .read_with(cx, |project, cx| project.kk_workbench_client(cx))
            .context("remote KK transport")?;
        let state = cx.new(|cx| {
            BlameState::new(
                BlameTarget::WorkingFile {
                    absolute,
                    remote: Some(remote),
                },
                &buffer,
                Diagnostics::default(),
                cx,
            )
        });
        state
            .update(cx, |state, cx| {
                state.enabled = true;
                state.load(cx);
                std::mem::replace(&mut state.task, Task::ready(()))
            })
            .await;
        state.read_with(cx, |state, _| {
            assert!(state.error.is_none(), "{:?}", state.error);
            assert!(
                state
                    .snapshot
                    .as_ref()
                    .expect("snapshot")
                    .repository()
                    .is_remote()
            );
            assert!(!state.lines.is_empty());
            assert!(!state.stale);
        });
        buffer.update(cx, |buffer, cx| {
            buffer.edit([(0..buffer.len(), "saved again\n")], None, cx);
        });
        assert!(state.read_with(cx, |state, _| state.stale));
        project
            .update(cx, |project, cx| project.save_buffer(buffer.clone(), cx))
            .await?;
        state
            .update(cx, |state, _| {
                std::mem::replace(&mut state.task, Task::ready(()))
            })
            .await;
        let snapshot = state.read_with(cx, |state, _| {
            assert!(state.error.is_none(), "{:?}", state.error);
            assert!(!state.stale);
            assert!(!state.lines.is_empty());
            state.snapshot.clone().expect("saved snapshot")
        });
        assert_eq!(
            snapshot.file_content("@", "file.txt").await?,
            b"saved again\n"
        );
        Ok(())
    }

    #[gpui::test(iterations = 3)]
    async fn historical_images_reuse_the_native_view_and_refresh_without_activating_it(
        cx: &mut TestAppContext,
    ) {
        cx.background_executor.allow_parking();
        let (directory, first, latest) = cx
            .background_spawn(async {
                futures_lite::future::block_on(async {
                    let directory = tempfile::tempdir()?;
                    run_kk(directory.path(), &["init", "."]).await?;
                    std::fs::write(directory.path().join("image.ppm"), b"P3\n1 1\n255\n0 0 0\n")?;
                    let repository = Repository::open(directory.path(), Path::new("kk")).await?;
                    let first = repository.snapshot(true).await?;
                    std::fs::write(
                        directory.path().join("image.ppm"),
                        b"P3\n2 1\n255\n0 0 0 255 0 0\n",
                    )?;
                    let latest = repository.snapshot(true).await?;
                    anyhow::Ok((directory, first, latest))
                })
            })
            .await
            .expect("image revisions");
        cx.update(|cx| {
            let settings = SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/project", serde_json::json!({})).await;
        let project = Project::test(fs, [Path::new("/project")], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |view, _| view.workspace().clone());
        let status = workspace.update(cx, |workspace, cx| {
            cx.new(|cx| KkBlameStatus::new(workspace, cx))
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace
                .status_bar()
                .update(cx, |bar, cx| bar.add_right_item(status.clone(), window, cx));
        });
        for _ in 0..2 {
            status
                .update_in(cx, |status, window, cx| {
                    status.open_file_task(
                        first.clone(),
                        "@".into(),
                        "image.ppm".into(),
                        false,
                        window,
                        cx,
                    )
                })
                .await
                .expect("open historical image");
        }

        let image = workspace.read_with(cx, |workspace, cx| {
            let images = workspace.items_of_type::<ImageView>(cx).collect::<Vec<_>>();
            assert_eq!(images.len(), 1);
            images[0].clone()
        });
        image.read_with(cx, |image, cx| {
            let metadata = image.image_metadata(cx).expect("historical metadata");
            assert_eq!((metadata.width, metadata.height), (1, 1));
            assert!(!workspace::Item::can_save(image, cx));
        });
        let split = cx
            .update(|window, cx| {
                image.update(cx, |image, cx| {
                    workspace::Item::clone_on_split(image, None, window, cx)
                })
            })
            .await
            .expect("split historical image");
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(Box::new(split.clone()), None, true, window, cx);
            workspace.active_pane().update(cx, |pane, cx| {
                pane.remove_item(image.entity_id(), false, false, window, cx)
            });
        });
        drop(image);
        let image = split;
        status
            .update_in(cx, |status, window, cx| {
                status.open_file_task(first, "@".into(), "image.ppm".into(), true, window, cx)
            })
            .await
            .expect("added image has an empty before version");
        let editor = workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_item_as::<Editor>(cx)
                .expect("empty before version")
        });
        assert_eq!(editor.read_with(cx, |editor, cx| editor.text(cx)), "");
        let pending = status.update(cx, |status, cx| {
            status.refresh(&latest, cx);
            status
                .images
                .values_mut()
                .map(|image| std::mem::replace(&mut image.task, Task::ready(())))
                .collect::<Vec<_>>()
        });
        for task in pending {
            task.await;
        }

        image.read_with(cx, |image, cx| {
            let metadata = image.image_metadata(cx).expect("updated metadata");
            assert_eq!((metadata.width, metadata.height), (2, 1));
        });
        assert_eq!(
            workspace.read_with(cx, |workspace, cx| workspace.active_item_as::<Editor>(cx)),
            Some(editor)
        );
        assert_eq!(
            std::fs::read(directory.path().join("image.ppm")).expect("working image"),
            b"P3\n2 1\n255\n0 0 0 255 0 0\n"
        );
    }

    #[gpui::test(iterations = 3)]
    async fn annotations_follow_versions_and_hide_during_edits(cx: &mut TestAppContext) {
        cx.background_executor.allow_parking();
        let (directory, first, latest) = cx
            .background_spawn(async move {
                futures_lite::future::block_on(async {
                    let directory = tempfile::tempdir()?;
                    let root = directory.path();
                    run_kk(root, &["init", "."]).await?;
                    run_kk(root, &["config", "set", "--repo", "user.name", "KK Author"]).await?;
                    run_kk(
                        root,
                        &[
                            "config",
                            "set",
                            "--repo",
                            "user.email",
                            "test@example.invalid",
                        ],
                    )
                    .await?;
                    run_kk(root, &["describe", "-m", "base"]).await?;
                    std::fs::write(root.join("file.txt"), "\u{feff}first\r\nsecond\r\n")?;
                    run_kk(root, &["new", "-m", "added line"]).await?;
                    std::fs::write(
                        root.join("file.txt"),
                        "\u{feff}first\r\nsecond\r\nthird\r\n",
                    )?;
                    let repository = Repository::open(root, Path::new("kk")).await?;
                    let first = repository.snapshot(true).await?;
                    std::fs::write(
                        root.join("file.txt"),
                        "\u{feff}first\r\nsecond\r\nthird\r\nfourth\r\n",
                    )?;
                    let latest = repository.snapshot(true).await?;
                    anyhow::Ok((directory, first, latest))
                })
            })
            .await
            .expect("prepare blame snapshots");
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            init(cx);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/project", serde_json::json!({})).await;
        let project = Project::test(fs, [Path::new("/project")], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |workspace, _| workspace.workspace().clone());
        let buffer = cx.new(|cx| {
            let mut buffer = Buffer::local("first\nsecond\nthird\n", cx);
            buffer.set_capability(Capability::ReadOnly, cx);
            buffer
        });
        let editor = cx.new_window_entity(|window, cx| {
            Editor::for_buffer(buffer.clone(), Some(project), window, cx)
        });
        let status = workspace.update(cx, |workspace, cx| {
            cx.new(|cx| KkBlameStatus::new(workspace, cx))
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace
                .status_bar()
                .update(cx, |bar, cx| bar.add_right_item(status.clone(), window, cx));
            workspace.add_item_to_active_pane(Box::new(editor.clone()), None, true, window, cx);
        });
        let state = status.update(cx, |status, cx| {
            status.attach(
                &editor,
                &buffer,
                BlameTarget::Revision {
                    snapshot: first,
                    revision: "@".into(),
                    path: "file.txt".into(),
                    refresh_buffer: true,
                    empty: false,
                },
                cx,
            )
        });
        status.update_in(cx, |status, window, cx| status.toggle(window, cx));
        state
            .update(cx, |state, _| {
                std::mem::replace(&mut state.task, Task::ready(()))
            })
            .await;
        cx.run_until_parked();
        state.read_with(cx, |state, _| {
            assert!(state.enabled);
            assert!(!state.loading);
            assert!(!state.stale, "{:?}", state.error);
            assert_eq!(state.lines.len(), 3);
            assert_eq!(state.lines[2].summary, "added line");
        });
        let column = editor.read_with(cx, |editor, cx| {
            editor
                .addon::<BlameAddon>()
                .expect("blame addon")
                .gutter_annotations(cx)
                .expect("visible annotations")
        });
        assert_eq!(cx.update(|_, cx| column.width_in_characters(cx)), 44);
        let width = editor.update_in(cx, |editor, window, cx| {
            let style = editor.style(cx).clone();
            let font = window.text_system().resolve_font(&style.text.font());
            editor
                .snapshot(window, cx)
                .gutter_dimensions(
                    font,
                    style.text.font_size.to_pixels(window.rem_size()),
                    &style,
                    window,
                    cx,
                )
                .git_blame_entries_width
                .expect("native gutter allocates annotation width")
        });
        assert!(width > gpui::px(100.));
        cx.update(|_, cx| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    let settings = settings
                        .kk_workbench
                        .get_or_insert_default()
                        .blame
                        .get_or_insert_default();
                    settings.column_width = Some(80);
                    settings.compact = Some(true);
                })
            })
        });
        assert_eq!(cx.update(|_, cx| column.width_in_characters(cx)), 82);
        let continuation =
            state.read_with(cx, |state, _| column_label(&state.lines[1], 80, true, 9));
        assert_eq!(continuation.trim_end_matches('\u{a0}'), "│");
        assert_eq!(continuation.chars().count(), 80);
        editor.update_in(cx, |editor, window, cx| {
            editor.change_selections(
                editor::SelectionEffects::no_scroll(),
                window,
                cx,
                |selections| {
                    selections
                        .select_ranges([language::Point::new(2, 0)..language::Point::new(2, 0)])
                },
            )
        });
        assert_eq!(
            status.read_with(cx, |status, cx| status
                .current_line(cx)
                .expect("selected line")
                .1
                .summary),
            "added line"
        );
        status.update_in(cx, |status, window, cx| {
            status.open_blamed_revision(window, cx)
        });
        status
            .update(cx, |status, _| {
                std::mem::replace(&mut status.open_task, Task::ready(()))
            })
            .await;
        let opened = workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_item_as::<Editor>(cx)
                .expect("historical editor")
        });
        assert_ne!(opened, editor);
        let opened_state = opened.read_with(cx, |editor, _| {
            editor
                .addon::<BlameAddon>()
                .expect("historical identity")
                .state
                .clone()
        });
        opened_state
            .update(cx, |state, _| {
                std::mem::replace(&mut state.task, Task::ready(()))
            })
            .await;
        assert!(!opened_state.read_with(cx, |state, _| state.enabled));
        assert_eq!(
            opened.read_with(cx, |editor, cx| editor.text(cx)),
            "first\nsecond\nthird\n"
        );
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.activate_item(&editor, true, true, window, cx);
        });
        status.update_in(cx, |status, window, cx| {
            status.open_blamed_revision(window, cx)
        });
        status
            .update(cx, |status, _| {
                std::mem::replace(&mut status.open_task, Task::ready(()))
            })
            .await;
        assert_eq!(
            workspace.read_with(cx, |workspace, cx| workspace.active_item_as::<Editor>(cx)),
            Some(opened)
        );
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.activate_item(&editor, true, true, window, cx);
        });
        state.update(cx, |state, cx| state.refresh(&latest, cx));
        state
            .update(cx, |state, _| {
                std::mem::replace(&mut state.task, Task::ready(()))
            })
            .await;
        cx.run_until_parked();
        state.read_with(cx, |state, _| {
            assert!(!state.stale, "{:?}", state.error);
            assert_eq!(state.lines.len(), 4);
        });
        assert_eq!(
            buffer.read_with(cx, |buffer, _| buffer.text()),
            "first\nsecond\nthird\nfourth\n"
        );

        let task = state.update(cx, |state, cx| {
            state.target = BlameTarget::WorkingFile {
                absolute: directory.path().join("file.txt"),
                remote: None,
            };
            state.load(cx);
            std::mem::replace(&mut state.task, Task::ready(()))
        });
        buffer.update(cx, |buffer, cx| {
            buffer.edit([(0..0, "new draft\n")], None, cx);
        });
        task.await;
        cx.run_until_parked();
        assert!(state.read_with(cx, |state, _| state.stale));
        assert!(
            buffer
                .read_with(cx, |buffer, _| buffer.text())
                .starts_with("new draft\n")
        );
        assert!(editor.read_with(cx, |editor, cx| {
            editor
                .addon::<BlameAddon>()
                .expect("blame addon")
                .gutter_annotations(cx)
                .is_none()
        }));
        assert!(status.read_with(cx, |status, cx| status.current_line(cx).is_none()));
        buffer.update(cx, |buffer, cx| {
            buffer.edit(
                [(0..buffer.len(), "first\nsecond\nthird\nfourth\n")],
                None,
                cx,
            );
            buffer.did_save(buffer.version(), None, cx);
        });
        state
            .update(cx, |state, _| {
                std::mem::replace(&mut state.task, Task::ready(()))
            })
            .await;
        cx.run_until_parked();
        assert!(!state.read_with(cx, |state, _| state.stale));
        let pending = state.update(cx, |state, cx| {
            state.load(cx);
            std::mem::replace(&mut state.task, Task::ready(()))
        });
        status.update_in(cx, |status, window, cx| status.toggle(window, cx));
        pending.await;
        assert!(!state.read_with(cx, |state, _| state.enabled));
        status.update_in(cx, |status, window, cx| {
            status.open_file(latest, "root()".into(), "new.txt".into(), true, window, cx)
        });
        status
            .update(cx, |status, _| {
                std::mem::replace(&mut status.open_task, Task::ready(()))
            })
            .await;
        let empty = workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_item_as::<Editor>(cx)
                .expect("empty before version")
        });
        let empty_state = empty.read_with(cx, |editor, _| {
            editor
                .addon::<BlameAddon>()
                .expect("empty revision identity")
                .state
                .clone()
        });
        empty_state
            .update(cx, |state, _| {
                std::mem::replace(&mut state.task, Task::ready(()))
            })
            .await;
        assert_eq!(empty.read_with(cx, |editor, cx| editor.text(cx)), "");
        assert!(empty.read_with(cx, |editor, cx| editor.read_only(cx)));
        assert!(empty_state.read_with(cx, |state, _| state.error.is_none()));
        drop(directory);
    }
}
