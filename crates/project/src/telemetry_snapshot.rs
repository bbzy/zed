use crate::Project;
use gpui::{App, Entity, Task};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TelemetrySnapshot {
    pub worktree_snapshots: Vec<TelemetryWorktreeSnapshot>,
}

impl TelemetrySnapshot {
    pub fn new(project: &Entity<Project>, cx: &mut App) -> Task<Self> {
        let worktree_snapshots = project
            .read(cx)
            .visible_worktrees(cx)
            .map(|worktree| TelemetryWorktreeSnapshot {
                worktree_path: worktree.read(cx).abs_path().to_string_lossy().into_owned(),
            })
            .collect();
        Task::ready(Self { worktree_snapshots })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TelemetryWorktreeSnapshot {
    pub worktree_path: String,
}
