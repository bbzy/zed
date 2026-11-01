use crate::{ChangedFile, DiffScope, Publication, Revision, Snapshot, transaction::Transaction};
use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum Category {
    Pending,
    Reviewed,
    Rejected,
    Debug,
}

impl Category {
    pub fn description(self) -> &'static str {
        match self {
            Self::Pending => "KKW: PENDING",
            Self::Reviewed => "KKW: REVIEWED",
            Self::Rejected => "KKW: REJECTED",
            Self::Debug => "KKW: DEBUG",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Pending => "Pending",
            Self::Reviewed => "Reviewed",
            Self::Rejected => "Rejected",
            Self::Debug => "Debug",
        }
    }

    pub fn from_revision(revision: &Revision) -> Option<Self> {
        [Self::Pending, Self::Reviewed, Self::Rejected, Self::Debug]
            .into_iter()
            .find(|category| revision.full_description.trim_end() == category.description())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CategoryCommit {
    pub category: Category,
    pub revision: Revision,
    pub files: Vec<ChangedFile>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum ClassificationTarget {
    Category(Category),
    Comparison,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SelectionSide {
    Original,
    Modified,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct LineRange {
    pub side: SelectionSide,
    pub begin: u32,
    pub end: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FileSelection {
    pub file: ChangedFile,
    pub ranges: Option<Vec<LineRange>>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ClassificationRequest {
    pub comparison: String,
    pub range_start: Option<String>,
    pub source: Option<Revision>,
    pub target: ClassificationTarget,
    pub files: Vec<FileSelection>,
}

pub struct PreparedClassification {
    transaction: crate::transaction::PreparedTransaction,
    destination: Revision,
    range_start: Revision,
}

#[derive(Debug)]
pub struct ClassificationResult {
    pub publication: Publication,
    pub destination: Revision,
    pub range_start: Revision,
}

#[derive(Serialize)]
struct RangePlan<'a> {
    from: String,
    into: String,
    files: Vec<RangeFile<'a>>,
}

#[derive(Serialize)]
struct RangeFile<'a> {
    path: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    ranges: Option<&'a [LineRange]>,
}

impl PreparedClassification {
    fn from_remote(prepared: crate::remote_client::RemotePrepared) -> Result<Self> {
        Ok(Self {
            destination: prepared
                .destination
                .clone()
                .context("Missing remote classification destination")?,
            range_start: prepared
                .range_start
                .clone()
                .context("Missing remote classification range")?,
            transaction: crate::transaction::PreparedTransaction::Remote(prepared),
        })
    }

    pub fn snapshot(&self) -> &Snapshot {
        self.transaction.snapshot()
    }

    pub fn destination(&self) -> &Revision {
        &self.destination
    }

    pub(crate) fn range_start(&self) -> &Revision {
        &self.range_start
    }

    pub(crate) async fn receipt(&self) -> Result<crate::OperationReceipt> {
        self.transaction
            .file_change_receipt(&[self.destination.revset(), self.range_start.revset()])
            .await
    }

    pub async fn publish(self) -> Result<ClassificationResult> {
        let publication = self
            .transaction
            .publish_file_change(&[self.destination.revset(), self.range_start.revset()])
            .await?;
        Ok(ClassificationResult {
            publication,
            destination: self.destination,
            range_start: self.range_start,
        })
    }
}

impl Snapshot {
    async fn classification_line_edits(
        &self,
        before: &Revision,
        after: &Revision,
        path: &str,
        original: Option<&str>,
        modified: Option<&str>,
    ) -> Result<Vec<crate::LineEdit>> {
        if original.is_none() || modified.is_none() {
            // Rename-aware diffs can pair this path with a different path's contents.
            let original = crate::selection::text_lines(original.unwrap_or_default()).len();
            let modified = crate::selection::text_lines(modified.unwrap_or_default()).len();
            return Ok(if original == 0 && modified == 0 {
                Vec::new()
            } else {
                vec![crate::LineEdit {
                    original: 0..original,
                    modified: 0..modified,
                }]
            });
        }
        let diff = self
            .read(&[
                "diff".into(),
                "--git".into(),
                "--context".into(),
                "0".into(),
                "--from".into(),
                before.full_commit_id.clone(),
                "--to".into(),
                after.full_commit_id.clone(),
                "--".into(),
                crate::repository::literal_fileset(path)?,
            ])
            .await?;
        crate::selection::native_line_edits(std::str::from_utf8(&diff)?, original, modified)
    }

    async fn classification_projections(
        &self,
        first: &Revision,
        head: &Revision,
        destination: &Revision,
        files: &[FileSelection],
        backward: bool,
    ) -> Result<Vec<(String, String, Option<Vec<u8>>)>> {
        let baseline = self
            .exact_revision(&format!("parents({})", first.revset()))
            .await?;
        let start = if backward { destination } else { &baseline };
        let mut chain = self
            .revisions(
                Some(&format!("{}::{}", start.revset(), head.revset())),
                usize::MAX,
            )
            .await?;
        chain.reverse();
        let base_index = chain
            .iter()
            .position(|revision| revision.full_change_id == baseline.full_change_id)
            .context("Missing classification baseline")?;
        let target_index = if backward {
            chain
                .len()
                .checked_sub(1)
                .context("Empty classification history")?
        } else {
            base_index
        };
        let target = chain
            .get(target_index)
            .context("Missing classification target")?;
        let mut history_files = Vec::new();
        for revision in &chain {
            history_files.extend(
                self.changed_files(&DiffScope::Change(revision.revset()))
                    .await?,
            );
        }
        let mut expected = Vec::new();
        let mut expanded = Vec::new();
        for selection in files {
            let mut paths = HashSet::from([selection.file.path.clone()]);
            if selection.ranges.is_none() {
                if let Some(original) = &selection.file.renamed_from {
                    paths.insert(original.clone());
                }
                loop {
                    let previous = paths.len();
                    for file in &history_files {
                        if let Some(original) = &file.renamed_from
                            && (paths.contains(&file.path) || paths.contains(original))
                        {
                            paths.insert(file.path.clone());
                            paths.insert(original.clone());
                        }
                    }
                    if paths.len() == previous {
                        break;
                    }
                }
            }
            for path in paths {
                let mut selection = selection.clone();
                selection.file.path = path;
                expanded.push(selection);
            }
        }
        for selection in &expanded {
            let path = &selection.file.path;
            let mut bytes = Vec::new();
            for revision in &chain {
                bytes.push(
                    self.file_content_if_exists(&revision.full_commit_id, path)
                        .await?,
                );
            }
            let texts = bytes
                .iter()
                .map(|value| {
                    value
                        .as_deref()
                        .map(std::str::from_utf8)
                        .transpose()
                        .map(|value| value.map(str::to_owned))
                })
                .collect::<std::result::Result<Vec<_>, _>>();
            let projected = if let Ok(texts) = texts
                && !bytes
                    .iter()
                    .flatten()
                    .any(|value| value.contains(&0) || crate::LfsPointer::parse(value).is_some())
            {
                let mut adjacent = Vec::new();
                for (index, pair) in chain.windows(2).enumerate() {
                    adjacent.push(
                        self.classification_line_edits(
                            &pair[0],
                            &pair[1],
                            path,
                            texts.get(index).and_then(Option::as_deref),
                            texts.get(index + 1).and_then(Option::as_deref),
                        )
                        .await?,
                    );
                }
                let mut to_target = Vec::new();
                for (index, revision) in chain.iter().enumerate() {
                    to_target.push(
                        self.classification_line_edits(
                            revision,
                            target,
                            path,
                            texts.get(index).and_then(Option::as_deref),
                            texts.get(target_index).and_then(Option::as_deref),
                        )
                        .await?,
                    );
                }
                let aggregate = self
                    .classification_line_edits(
                        &baseline,
                        head,
                        path,
                        texts.get(base_index).and_then(Option::as_deref),
                        texts.last().and_then(Option::as_deref),
                    )
                    .await?;
                crate::selection::project_history(
                    &texts,
                    &adjacent,
                    &aggregate,
                    &to_target,
                    base_index,
                    selection.ranges.as_deref(),
                    !backward,
                )?
                .into_iter()
                .map(|value| value.map(String::into_bytes))
                .collect::<Vec<_>>()
            } else {
                ensure!(
                    selection.ranges.is_none(),
                    "Binary files require whole-file selection"
                );
                let baseline_bytes = bytes.get(base_index).context("Missing binary baseline")?;
                let target_bytes = bytes.get(target_index).context("Missing binary target")?;
                bytes
                    .iter()
                    .enumerate()
                    .map(|(index, value)| {
                        if backward && index < base_index && baseline_bytes == target_bytes {
                            value.clone()
                        } else {
                            target_bytes.clone()
                        }
                    })
                    .collect()
            };
            for (index, (revision, content)) in chain.iter().zip(projected).enumerate() {
                if !backward && index == base_index {
                    continue;
                }
                expected.push((revision.revset(), path.clone(), content));
            }
        }
        Ok(expected)
    }

    pub async fn rebase_classification(
        &self,
        previous: &Snapshot,
        request: &ClassificationRequest,
    ) -> Result<Option<ClassificationRequest>> {
        if let Some(remote) = self.repository().remote() {
            ensure!(
                self.repository().same_location(previous.repository()),
                "Snapshots belong to different repositories"
            );
            return remote
                .read(
                    self,
                    crate::remote::Read::RebaseClassification {
                        previous: remote.info(previous),
                        request: request.clone(),
                    },
                )
                .await;
        }

        self.rebase_classification_with_inputs(previous, request, None)
            .await
    }

    async fn rebase_classification_with_inputs(
        &self,
        previous: &Snapshot,
        request: &ClassificationRequest,
        inputs: Option<&std::collections::HashMap<String, crate::SavedFile>>,
    ) -> Result<Option<ClassificationRequest>> {
        ensure!(
            self.repository().root() == previous.repository().root(),
            "Classification snapshots belong to different repositories"
        );
        if self.operation() == previous.operation() {
            return Ok(Some(request.clone()));
        }

        let mut updated = request.clone();
        if let Some(source) = &request.source {
            updated.source = Some(self.exact_revision(&source.revset()).await?);
        }

        let previous_scope = previous.classification_scope(request).await?;
        let scope = self.classification_scope(&updated).await?;
        let files = self.changed_files(&scope).await?;
        updated.files.clear();
        for selection in &request.files {
            let Some(file) = files.iter().find(|file| file.path == selection.file.path) else {
                continue;
            };
            ensure!(
                file.renamed_from == selection.file.renamed_from,
                "The selected file's rename relationship changed while classification was queued"
            );
            let ranges = if let Some(ranges) = &selection.ranges {
                let (original, mut modified) = previous
                    .classification_text(&previous_scope, &selection.file.path)
                    .await?;
                if let Some(input) = inputs.and_then(|inputs| inputs.get(&selection.file.path)) {
                    modified =
                        String::from_utf8(input.bytes.as_deref().unwrap_or_default().to_vec())?;
                }

                let (next_original, next_modified) = self
                    .classification_text(&scope, &selection.file.path)
                    .await?;
                let projection = crate::projection_between(&original, &modified, &next_original, &next_modified)?
                    .context("This diff is too large to relocate queued lines. Reopen the diff and select the changes again.")?;
                let selected = crate::selection::combined_selection(ranges, &projection)?;
                let selected = crate::rebase_selection(&selected, &projection)?;
                let modified_ids = projection
                    .modified_destination_ids
                    .unwrap_or(projection.modified_ids);
                let remaining = crate::SelectionProjection {
                    atoms: crate::selection::lineage_atoms(
                        &projection.remaining_edits,
                        &projection.destination_ids,
                        &modified_ids,
                    )?,
                    edits: projection.remaining_edits.clone(),
                    remaining_edits: projection.remaining_edits,
                    original_ids: projection.destination_ids.clone(),
                    destination_ids: projection.destination_ids,
                    modified_ids,
                    modified_destination_ids: None,
                };
                let ranges = crate::native_selection_ranges(&selected, &remaining)?;
                if ranges.is_empty() {
                    continue;
                }

                Some(ranges)
            } else {
                None
            };
            updated.files.push(FileSelection {
                file: file.clone(),
                ranges,
            });
        }

        Ok((!updated.files.is_empty()).then_some(updated))
    }

    async fn classification_scope(&self, request: &ClassificationRequest) -> Result<DiffScope> {
        match &request.source {
            Some(source) => Ok(DiffScope::Change(source.full_commit_id.clone())),
            None => Ok(DiffScope::Comparison {
                base: self
                    .comparison_boundary(&request.comparison, request.range_start.as_deref())
                    .await?
                    .base_revset,
                target: "@".into(),
            }),
        }
    }

    async fn classification_text(&self, scope: &DiffScope, path: &str) -> Result<(String, String)> {
        let (base, head) = match scope {
            DiffScope::Change(revision) => (format!("parents({revision})"), revision.clone()),
            DiffScope::Comparison { base, target } => (base.clone(), target.clone()),
        };
        let original = self
            .file_content_if_exists(&base, path)
            .await?
            .unwrap_or_default();
        let modified = self
            .file_content_if_exists(&head, path)
            .await?
            .unwrap_or_default();
        ensure!(
            !original.contains(&0) && !modified.contains(&0),
            "Binary files require a whole-file selection"
        );
        Ok((String::from_utf8(original)?, String::from_utf8(modified)?))
    }

    pub async fn category_commits(&self, range_start: &Revision) -> Result<Vec<CategoryCommit>> {
        if let Some(remote) = self.repository().remote() {
            return remote
                .read(
                    self,
                    crate::remote::Read::CategoryCommits {
                        range_start: range_start.clone(),
                    },
                )
                .await;
        }

        let mut cursor = self.exact_revision(&range_start.revset()).await?;
        let mut commits = Vec::new();
        while cursor.parent_commit_ids.len() == 1 {
            let parent = self
                .exact_revision(&format!("parents({})", cursor.full_commit_id))
                .await?;
            let Some(category) = Category::from_revision(&parent) else {
                break;
            };
            if parent.parent_commit_ids.len() != 1 {
                break;
            }

            let files = self
                .changed_files(&DiffScope::Change(parent.full_commit_id.clone()))
                .await?;
            commits.push(CategoryCommit {
                category,
                revision: parent.clone(),
                files,
            });
            cursor = parent;
        }

        commits.reverse();
        Ok(commits)
    }

    pub async fn exact_revision(&self, revset: &str) -> Result<Revision> {
        if let Some(remote) = self.repository().remote() {
            return remote
                .read(
                    self,
                    crate::remote::Read::ExactRevision {
                        revset: revset.into(),
                    },
                )
                .await;
        }

        self.revisions(Some(&format!("exactly(({revset}), 1)")), 2)
            .await
            .with_context(|| format!("Could not resolve revision {revset}"))?
            .pop()
            .context("Revision does not resolve uniquely")
    }

    pub async fn prepare_classification(
        &self,
        request: &ClassificationRequest,
    ) -> Result<PreparedClassification> {
        if self.repository().is_remote() {
            return self
                .prepare_classification_with_saved(self, request, Vec::new())
                .await?
                .context("Classification has already been applied");
        }

        self.prepare_classification_in(request, None).await
    }

    pub async fn prepare_classification_with_saved(
        &self,
        previous: &Snapshot,
        request: &ClassificationRequest,
        saved: Vec<crate::SavedSnapshot>,
    ) -> Result<Option<PreparedClassification>> {
        if let Some(remote) = self.repository().remote() {
            ensure!(
                self.repository().same_location(previous.repository()),
                "Snapshots belong to different repositories"
            );
            let mut captures = Vec::with_capacity(saved.len());
            for capture in &saved {
                captures.push(capture.remote_id(self.repository()).await?);
            }

            return remote
                .prepare(
                    self,
                    crate::remote::Mutation::Classify {
                        previous: remote.info(previous),
                        request: request.clone(),
                        saved: captures,
                    },
                )
                .await?
                .map(PreparedClassification::from_remote)
                .transpose();
        }

        let files = crate::SavedSnapshot::combined_files(&saved).await?;
        let mut transaction = Transaction::begin_with_saved(self, saved).await?;
        transaction.prioritize_diff_opening();
        let working_copy = request
            .source
            .as_ref()
            .is_none_or(|source| source.is_working_copy);
        if working_copy {
            ensure!(
                previous.exact_revision("@").await?.full_change_id
                    == self.exact_revision("@").await?.full_change_id,
                "The working-copy change was switched while classification was pending"
            );
            let scope = previous.classification_scope(request).await?;
            let base = match scope {
                DiffScope::Change(revision) => format!("parents({revision})"),
                DiffScope::Comparison { base, .. } => base,
            };
            for (path, saved) in &files {
                if saved.bytes.is_some() && self.file_content_if_exists("@", path).await?.is_none()
                {
                    ensure!(
                        previous
                            .file_content_if_exists(&base, path)
                            .await?
                            .is_some(),
                        "The saved file {path} has not entered the comparison yet. Refresh after pending classifications finish."
                    );
                }
            }

            transaction.prepare_inputs(&files).await?;
        }

        let prepared = transaction.snapshot().clone();
        let Some(request) = prepared
            .rebase_classification_with_inputs(previous, request, working_copy.then_some(&files))
            .await?
        else {
            return Ok(None);
        };
        prepared
            .prepare_classification_in(&request, Some(transaction))
            .await
            .map(Some)
    }

    async fn prepare_classification_in(
        &self,
        request: &ClassificationRequest,
        transaction: Option<Transaction>,
    ) -> Result<PreparedClassification> {
        ensure!(
            !request.files.is_empty(),
            "Select at least one changed file"
        );
        let mut paths = HashSet::new();
        for selection in &request.files {
            validate_selection_path(&selection.file.path)?;
            ensure!(
                paths.insert(selection.file.path.clone()),
                "A file occurs more than once in this batch"
            );
            if let Some(source) = &selection.file.renamed_from {
                validate_selection_path(source)?;
            }

            if let Some(ranges) = &selection.ranges {
                ensure!(
                    !ranges.is_empty()
                        && ranges
                            .iter()
                            .all(|range| range.begin > 0 && range.begin <= range.end),
                    "Line selections must use nonempty, one-based inclusive ranges"
                );
                ensure!(
                    selection.file.renamed_from.is_none(),
                    "Use a whole-file selection for renamed or copied files"
                );
            }
        }

        let help = self.repository().command(&["squash-range", "--help"]).await
            .context("Classification requires KK squash-range --json-file. Update the configured KK executable.")?;
        ensure!(
            String::from_utf8_lossy(&help.stdout).contains("--json-file"),
            "Classification requires KK squash-range --json-file. Update the configured KK executable."
        );
        let mut transaction = match transaction {
            Some(transaction) => transaction,
            None => Transaction::begin(self).await?,
        };
        transaction.prioritize_diff_opening();
        let boundary = self
            .comparison_boundary(&request.comparison, request.range_start.as_deref())
            .await?;
        let range_start = boundary
            .range_start
            .context("Classification requires a continuous ancestor comparison range")?;
        let categories = self.category_commits(&range_start).await?;
        let category_scope = std::iter::once(range_start.revset())
            .chain(categories.iter().map(|entry| entry.revision.revset()))
            .collect::<Vec<_>>()
            .join(" | ");
        ensure!(
            categories.iter().all(|entry| !entry.revision.has_conflict)
                && self
                    .revisions(Some(&format!("({category_scope}) ~ mutable()")), 1)
                    .await?
                    .is_empty(),
            "The comparison range start and category stack must be mutable and conflict-free"
        );
        let working_copy = self.exact_revision("@").await?;
        let head = match &request.source {
            Some(source) => self.exact_revision(&source.revset()).await?,
            None => working_copy.clone(),
        };
        if let Some(source) = &request.source {
            ensure!(
                head.full_commit_id == source.full_commit_id,
                "The selected source changed. Refresh before classifying it."
            );
        }

        let source_category = Category::from_revision(&head);
        let base = if source_category.is_some() {
            ensure!(
                request.source.is_some()
                    && categories
                        .iter()
                        .any(|category| category.revision.full_change_id == head.full_change_id),
                "This category does not belong to the selected comparison range"
            );
            if let ClassificationTarget::Category(category) = request.target {
                ensure!(
                    Some(category) != source_category,
                    "Changes already belong to this category"
                );
            }

            self.exact_revision(&format!("parents({})", head.full_commit_id))
                .await?
        } else {
            ensure!(
                request.target != ClassificationTarget::Comparison,
                "Only classified changes can be returned to Comparison"
            );
            ensure!(
                !self
                    .revisions(
                        Some(&format!(
                            "{} & {}::@",
                            head.full_commit_id, range_start.full_commit_id
                        )),
                        1
                    )
                    .await?
                    .is_empty(),
                "The selected change does not belong to this comparison range"
            );
            if request.source.is_some() {
                self.exact_revision(&format!("parents({})", head.full_commit_id))
                    .await?
            } else {
                self.exact_revision(&boundary.base_revset).await?
            }
        };
        ensure!(
            !base.has_conflict,
            "Resolve conflicts before classifying changes"
        );
        let mut sources = self
            .revisions(
                Some(&format!("{}..{}", base.full_commit_id, head.full_commit_id)),
                usize::MAX,
            )
            .await?;
        sources.reverse();
        let mut parent = &base.full_commit_id;
        for source in &sources {
            ensure!(
                !source.has_conflict && source.parent_commit_ids.as_slice() == [parent.clone()],
                "Classification requires a conflict-free linear source range"
            );
            parent = &source.full_commit_id;
        }

        ensure!(
            !sources.is_empty() && parent == &head.full_commit_id,
            "The source range is empty or disconnected"
        );

        let files = self
            .changed_files(&DiffScope::Comparison {
                base: base.full_commit_id.clone(),
                target: head.full_commit_id.clone(),
            })
            .await?;
        for selection in &request.files {
            ensure!(
                files.contains(&selection.file),
                "The selected file or its rename relationship changed: {}",
                selection.file.path
            );
        }

        // A rename can cross temporary paths inside the range. They are part of the selected move.
        for source in sources.iter().rev() {
            for file in self
                .changed_files(&DiffScope::Change(source.full_commit_id.clone()))
                .await?
            {
                if file.change_type == crate::ChangeKind::Renamed
                    && paths.contains(&file.path)
                    && let Some(original) = file.renamed_from
                {
                    paths.insert(original);
                }
            }
        }

        for selection in &request.files {
            if selection.file.change_type == crate::ChangeKind::Renamed
                && let Some(original) = &selection.file.renamed_from
            {
                paths.insert(original.clone());
            }
        }

        let earliest = categories
            .first()
            .map(|category| &category.revision)
            .unwrap_or(&range_start);
        let descendants = self
            .revisions(
                Some(&format!("descendants({})", earliest.full_commit_id)),
                usize::MAX,
            )
            .await?;
        let source_first = sources.first().context("Missing first source")?;
        let destination = match request.target {
            ClassificationTarget::Comparison => Some(working_copy.clone()),
            ClassificationTarget::Category(category) => categories
                .iter()
                .find(|entry| entry.category == category)
                .map(|entry| entry.revision.clone()),
        };
        let destination = if let Some(destination) = destination {
            destination
        } else {
            let ClassificationTarget::Category(category) = request.target else {
                anyhow::bail!("Missing comparison destination");
            };
            let insert_before = if source_category.is_some() {
                &head
            } else {
                &range_start
            };
            transaction
                .prepare(&[
                    "new",
                    "--no-edit",
                    "--insert-before",
                    &insert_before.revset(),
                    "--message",
                    category.description(),
                ])
                .await?;
            transaction
                .snapshot()
                .exact_revision(&format!("parents({})", insert_before.revset()))
                .await?
        };
        let destination_before_source = !transaction
            .snapshot()
            .revisions(
                Some(&format!(
                    "{} & ::{}",
                    destination.revset(),
                    source_first.revset()
                )),
                1,
            )
            .await?
            .is_empty();
        let (first, last) = if destination_before_source {
            (&destination, &head)
        } else {
            (source_first, &destination)
        };
        let affected = transaction
            .snapshot()
            .revisions(
                Some(&format!("{}::{}", first.revset(), last.revset())),
                usize::MAX,
            )
            .await?;
        ensure!(
            affected
                .iter()
                .all(|revision| !revision.has_conflict && revision.parent_commit_ids.len() == 1),
            "Classification requires mutable, conflict-free linear history between source and destination"
        );
        let affected_ids: HashSet<_> = affected
            .iter()
            .map(|revision| revision.full_change_id.as_str())
            .collect();

        let mut plan = RangePlan {
            from: format!("{}::{}", source_first.revset(), head.revset()),
            into: destination.revset(),
            files: request
                .files
                .iter()
                .map(|selection| RangeFile {
                    path: &selection.file.path,
                    ranges: selection.ranges.as_deref(),
                })
                .collect(),
        };
        let expected_files = transaction
            .snapshot()
            .classification_projections(
                source_first,
                &head,
                &destination,
                &request.files,
                destination_before_source,
            )
            .await?;
        plan.files.sort_by_key(|file| file.path);
        let encoded = serde_json::to_vec(&plan)?;
        let directory = smol::unblock(|| {
            tempfile::Builder::new()
                .prefix("zed-kk-classification-")
                .tempdir()
        })
        .await?;
        let plan_path = directory.path().join("selection.json");
        smol::fs::write(&plan_path, encoded).await?;
        let preparation = transaction
            .prepare(&[
                "squash-range",
                "--json-file",
                plan_path.to_str().context("Temporary path is not UTF-8")?,
            ])
            .await;
        smol::unblock(move || directory.close()).await?;
        preparation?;

        for (revision, path, expected) in expected_files {
            let actual = transaction
                .snapshot()
                .file_content_if_exists(&revision, &path)
                .await?;
            ensure!(
                actual == expected,
                "Classification changed the projected content of {path} in {revision}. Nothing was published."
            );
        }

        for before in descendants {
            let after = transaction
                .snapshot()
                .exact_revision(&before.revset())
                .await?;
            ensure!(
                before.full_description == after.full_description
                    && before.author_name == after.author_name
                    && before.author_email == after.author_email
                    && (!after.has_conflict || before.has_conflict),
                "Classification changed unrelated metadata or introduced a conflict. Nothing was published."
            );
            let changes = transaction
                .snapshot()
                .changed_files(&DiffScope::Comparison {
                    base: before.full_commit_id.clone(),
                    target: after.full_commit_id,
                })
                .await?;
            if !affected_ids.contains(before.full_change_id.as_str())
                || before.is_working_copy
                || before.full_change_id == last.full_change_id
            {
                ensure!(
                    changes.is_empty(),
                    "Classification changed a protected descendant snapshot. Nothing was published."
                );
            } else {
                ensure!(
                    changes.iter().all(|file| {
                        paths.contains(&file.path)
                            && file
                                .renamed_from
                                .as_ref()
                                .is_none_or(|path| paths.contains(path))
                    }),
                    "Classification changed an unselected path. Nothing was published."
                );
            }
        }

        transaction.restore_inputs().await?;
        let destination = transaction
            .snapshot()
            .exact_revision(&destination.revset())
            .await?;
        let range_start = transaction
            .snapshot()
            .exact_revision(&range_start.revset())
            .await?;
        Ok(PreparedClassification {
            transaction: transaction.into(),
            destination,
            range_start,
        })
    }
}

pub(crate) fn validate_selection_path(path: &str) -> Result<()> {
    crate::repository::validate_path(path)?;
    ensure!(
        !crate::repository::is_metadata_path(path),
        "Repository metadata cannot be classified"
    );
    Ok(())
}
