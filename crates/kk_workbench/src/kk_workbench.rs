mod classification;
mod conflict;
mod conflict_model;
mod diagnostics;
mod discovery;
mod file;
mod file_mutation;
mod history;
mod opening;
mod preview;
pub mod remote;
mod remote_client;
mod repository;
mod saved;
mod search;
mod selection;
mod transaction;

pub use classification::{
    Category, CategoryCommit, ClassificationRequest, ClassificationResult, ClassificationTarget,
    FileSelection, LineRange, PreparedClassification, SelectionSide,
};
pub use conflict::{
    ConflictHunk, ConflictInput, ConflictRole, ConflictSection, ConflictSource, FileConflict,
    PreparedConflictResolution, conflict_marker_length, parse_conflict_hunks,
    parse_conflict_inputs,
};
pub use conflict_model::{
    ConflictAcceptance, ConflictChoice, ConflictDraft, ConflictEdit, ConflictEditReason,
    PresentedConflict, PresentedSection, normalize_conflict_text,
};
pub use diagnostics::{Diagnostics, PerformanceContext, PerformanceSpan};
pub use discovery::{Discovery, DiscoveryError, discover, discover_with_diagnostics};
pub use file::{BlameLine, FileInformation, LfsPointer, SparseCheckout};
pub use file_mutation::{FileMoveDirection, PreparedFileMutation};
pub use history::{GraphRow, HistoryMode, HistoryPage, Revision, history_graph, rank_revisions};
pub use opening::DiffOpening;
pub use preview::{ClassificationPreview, TransferDirection};
pub use repository::{
    ChangeKind, ChangedFile, ComparisonBoundary, DiffScope, Repository, RepositoryLifetime,
    Snapshot, is_stale_workspace,
};
pub use saved::{SavedFile, SavedFileWrite, SavedSnapshot, record_saved_file};
pub use search::{
    ChangedLine, LineKind, SearchOptions, SearchResults, parse_git_diff, search_diff,
};
pub use selection::{
    FileStep, LineAnchor, LineAtom, LineEdit, LineSelection, SelectionPlan, SelectionPlanOptions,
    SelectionProjection, local_line_edits, native_selection_ranges, plan_selection,
    projection_between, rebase_selection, transfer_text,
};
pub use transaction::{OperationReceipt, Publication, PublicationStatus, UncertainPublication};

#[cfg(test)]
mod tests;
