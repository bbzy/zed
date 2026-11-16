use crate::{
    Buffer, Capability, DiskState, File, LanguageRegistry, LineEnding, LocalFile, ReplicaId, Rope,
    TextBuffer,
};
use anyhow::Result;
use gpui::{App, AppContext as _, AsyncWindowContext, Entity};
use settings::WorktreeId;
use std::{path::PathBuf, sync::Arc};
use util::{ResultExt as _, paths::PathStyle, rel_path::RelPath};

pub struct HistoricalFile {
    pub path: Arc<RelPath>,
    pub worktree_id: WorktreeId,
    pub is_deleted: bool,
    pub is_binary: bool,
    pub display_name: String,
}

impl File for HistoricalFile {
    fn as_local(&self) -> Option<&dyn LocalFile> {
        None
    }

    fn disk_state(&self) -> DiskState {
        DiskState::Historic {
            was_deleted: self.is_deleted,
        }
    }

    fn path_style(&self, _: &App) -> PathStyle {
        PathStyle::local()
    }

    fn path(&self) -> &Arc<RelPath> {
        &self.path
    }

    fn full_path(&self, _: &App) -> PathBuf {
        if self.path.is_empty() {
            PathBuf::from(".")
        } else {
            self.path.as_std_path().to_path_buf()
        }
    }

    fn file_name<'a>(&'a self, _: &'a App) -> &'a str {
        self.display_name.as_ref()
    }

    fn worktree_id(&self, _: &App) -> WorktreeId {
        self.worktree_id
    }

    fn to_proto(&self, _cx: &App) -> crate::proto::File {
        unimplemented!()
    }

    fn is_private(&self) -> bool {
        false
    }

    fn can_open(&self) -> bool {
        !self.is_binary
    }
}

pub async fn build_buffer(
    mut text: String,
    blob: Arc<dyn File>,
    language_registry: &Arc<LanguageRegistry>,
    cx: &mut AsyncWindowContext,
) -> Result<Entity<Buffer>> {
    let line_ending = LineEnding::detect(&text);
    LineEnding::normalize(&mut text);
    let text = Rope::from(text);
    let language =
        cx.update(|_, cx| language_registry.language_for_file(&blob, Some(&text), cx))?;
    let language = if let Some(language_id) = language {
        language_registry
            .load_language(language_id)
            .await
            .ok()
            .and_then(|e| e.log_err())
    } else {
        None
    };
    let buffer = cx.new(|cx| {
        let buffer = TextBuffer::new_normalized(
            ReplicaId::LOCAL,
            cx.entity_id().as_non_zero_u64().into(),
            line_ending,
            text,
        );
        let mut buffer = Buffer::build(buffer, Some(blob), Capability::ReadWrite, cx);
        buffer.set_language_registry(language_registry.clone());
        buffer.set_language_async(language, cx);
        buffer
    });
    Ok(buffer)
}
