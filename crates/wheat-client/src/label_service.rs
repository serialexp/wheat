//! Native short-label inspection and compare-and-swap mutation.

use std::{fmt::Write as _, path::Path, str::FromStr};

use anyhow::{Context, Result};
use serde::Serialize;
use wheat_core::{
    LabelAnnotation, MAX_LABELS, RepositoryEnvelopeId, ViewId, ViewLabel, ViewLabelManifestRecord,
};
use wheat_store::{
    MergeBudget, PublicationStore, RepositoryHead, RepositoryPublisher, RepositoryReader,
    RetainedSetStore,
};

use crate::{
    authoring_metadata::display_timestamp,
    label_move,
    local::{LocalRepository, runtime},
    outcome::{CliFailure, Success},
    workspace_metadata::{PendingRepositoryOperation, WorkspaceMetadata},
    workspace_registry_service,
};

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct LabelEntry {
    pub name: String,
    pub view: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub original_git_ref: Option<String>,
    /// Present when the label is published as an annotated Git tag.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub annotation: Option<AnnotationDocument>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct AnnotationDocument {
    pub tagger_name: String,
    pub tagger_email: String,
    pub tagged_at: String,
    pub message: String,
}

impl LabelEntry {
    fn new(name: String, label: &ViewLabel) -> Result<Self> {
        Ok(Self {
            name,
            view: label.view.to_string(),
            original_git_ref: label.original_git_ref.clone(),
            annotation: label
                .annotation
                .as_ref()
                .map(annotation_document)
                .transpose()?,
        })
    }
}

fn annotation_document(annotation: &LabelAnnotation) -> Result<AnnotationDocument> {
    Ok(AnnotationDocument {
        tagger_name: annotation.tagger.name.clone(),
        tagger_email: annotation.tagger.email.clone(),
        tagged_at: display_timestamp(annotation.tagged_at)?,
        message: annotation.message.clone(),
    })
}

#[derive(Debug, Serialize)]
pub struct LabelList {
    pub schema_version: u32,
    pub head: HeadDocument,
    pub labels: Vec<LabelEntry>,
}

#[derive(Clone, Debug, Serialize)]
pub struct HeadDocument {
    pub generation: u64,
    pub envelope: String,
}

impl From<RepositoryHead> for HeadDocument {
    fn from(head: RepositoryHead) -> Self {
        Self {
            generation: head.generation,
            envelope: head.envelope.to_string(),
        }
    }
}

pub fn parse_expected_head(value: &str) -> Result<RepositoryHead> {
    let (generation, envelope) = value.split_once(':').ok_or_else(|| {
        CliFailure::invalid("expected head must have the form <generation>:<envelope>")
    })?;
    Ok(RepositoryHead {
        generation: generation
            .parse()
            .map_err(|_| CliFailure::invalid("invalid expected head generation"))?,
        envelope: RepositoryEnvelopeId::from_str(envelope)
            .map_err(|_| CliFailure::invalid("invalid expected head envelope"))?,
    })
}

pub fn list(repository: &LocalRepository) -> Result<Success> {
    let document = runtime()?.block_on(load(repository))?;
    let mut human = String::new();
    for label in &document.labels {
        write!(human, "{}\t{}", label.name, label.view)?;
        if let Some(original) = &label.original_git_ref {
            write!(human, "\t{original}")?;
        }
        if let Some(annotation) = &label.annotation {
            let title = annotation.message.lines().next().unwrap_or_default();
            write!(
                human,
                "\ttagged by {} <{}> at {}: {title}",
                annotation.tagger_name, annotation.tagger_email, annotation.tagged_at
            )?;
        }
        human.push('\n');
    }
    Success::new("labels", &document, human)
}

/// Creates a label; with an `annotation` it can be pushed as an annotated Git tag.
pub fn create(
    repository: &LocalRepository,
    name: String,
    view: ViewId,
    annotation: Option<LabelAnnotation>,
    expected: Option<RepositoryHead>,
) -> Result<Success> {
    mutate(repository, expected, "label.create", move |labels| {
        if labels.contains_key(&name) {
            return Err(CliFailure::invalid(format!("label {name} already exists")));
        }
        if labels.len() == MAX_LABELS {
            return Err(CliFailure::limit(format!(
                "repository cannot contain more than {MAX_LABELS} labels"
            )));
        }
        let label = ViewLabel {
            annotation,
            ..ViewLabel::from(view)
        };
        let entry = LabelEntry::new(name.clone(), &label)?;
        labels.insert(name, label);
        Ok(entry)
    })
}

pub fn rename(
    repository: &LocalRepository,
    old: String,
    new: String,
    expected: Option<RepositoryHead>,
) -> Result<Success> {
    mutate(repository, expected, "label.rename", move |labels| {
        if labels.contains_key(&new) {
            return Err(CliFailure::invalid(format!("label {new} already exists")));
        }
        let label = labels
            .remove(&old)
            .ok_or_else(|| CliFailure::invalid(format!("label {old} does not exist")))?;
        let entry = LabelEntry::new(new.clone(), &label)?;
        labels.insert(new, label);
        Ok(entry)
    })
}

pub fn delete(
    repository: &LocalRepository,
    name: String,
    expected: Option<RepositoryHead>,
) -> Result<Success> {
    mutate(repository, expected, "label.delete", move |labels| {
        let label = labels
            .remove(&name)
            .ok_or_else(|| CliFailure::invalid(format!("label {name} does not exist")))?;
        LabelEntry::new(name, &label)
    })
}

/// The workspace a label move runs in: locked, and recovered from any interrupted
/// operation.
pub struct MovingWorkspace<'a> {
    pub root: &'a Path,
    pub store_root: &'a Path,
    pub metadata: &'a mut WorkspaceMetadata,
}

/// Moves label `name` to view `to`, which the head must retain. Unless `force`, the
/// move must be forward and leave no frontier of the label's view behind. Moving the
/// workspace's active label to its effective view rebinds the workspace there; the
/// rebind is staged in its metadata before the head moves, so an interruption on either
/// side of the CAS leaves an operation the next command completes or discards.
pub fn move_label(
    repository: &LocalRepository,
    name: String,
    to: ViewId,
    force: bool,
    expected: Option<RepositoryHead>,
    workspace: Option<MovingWorkspace<'_>>,
) -> Result<Success> {
    let runtime = runtime()?;
    let observed = repository.heads.read()?;
    let expected = expected.unwrap_or(observed);
    ensure_expected(expected, observed)?;
    let reader = RepositoryReader::new(&repository.objects, &repository.content);
    let (state_id, mut labels, from) = runtime.block_on(async {
        let envelope = reader.envelope(expected.envelope).await?;
        let state = reader.state(envelope.state).await?;
        let labels = reader.view_label_manifest(envelope.labels).await?.labels;
        let from = labels
            .get(&name)
            .map(|label| label.view)
            .ok_or_else(|| CliFailure::invalid(format!("label {name} does not exist")))?;
        if !RetainedSetStore::new(&repository.objects)
            .contains(state.retained_views, to.into())
            .await?
        {
            return Err(CliFailure::invalid(format!(
                "view {to} is not retained in this repository"
            )));
        }
        if !force {
            let mut budget = MergeBudget::STANDARD;
            let refusal =
                label_move::check(&reader, &repository.objects, &state, from, to, &mut budget)
                    .await?;
            if !refusal.is_empty() {
                return Err(CliFailure::label_move_refused(format!(
                    "cannot move label {name} to view {to}: {}; pass --force to move it anyway",
                    refusal.describe(&name, to)
                )));
            }
        }
        Ok::<_, anyhow::Error>((envelope.state, labels, from))
    })?;
    if from == to {
        let entry = LabelEntry::new(name.clone(), &labels[&name])?;
        return Success::new(
            "label.move",
            serde_json::json!({
                "schema_version": 1,
                "head": HeadDocument::from(expected),
                "label": entry,
                "moved": false,
                "from": from.to_string(),
                "rebound": false,
            }),
            format!("label {name} already names view {to}\n"),
        );
    }
    let label = labels.get_mut(&name).expect("the label was read above");
    label.view = to;
    let entry = LabelEntry::new(name.clone(), label)?;
    let manifest = ViewLabelManifestRecord::new(labels).context("invalid native label manifest")?;
    let publisher =
        RepositoryPublisher::new(&repository.objects, &repository.content, &repository.heads);
    let root = runtime.block_on(publisher.stage_envelope(state_id, &manifest))?;
    let rebinding = workspace.filter(|workspace| {
        workspace.metadata.active_label == name && workspace.metadata.effective_view() == to
    });
    let head = match rebinding {
        Some(workspace) => {
            let metadata = &mut *workspace.metadata;
            metadata.pending_repository_operation = Some(PendingRepositoryOperation {
                expected_head: expected.into(),
                proposed_head: expected.next(root)?.into(),
                resulting_frontier: to,
                selected_change: metadata.selected_change,
                rebinds: true,
            });
            workspace_registry_service::reconcile(workspace.root, workspace.store_root, metadata)?;
            metadata
                .replace_prepared(workspace.root)?
                .publish()
                .map_err(|error| anyhow::anyhow!(error))?;
            crate::materialization::checkpoint("label-move-staged")?;
            let head = runtime.block_on(publisher.publish_envelope(expected, root))?;
            crate::materialization::checkpoint("label-move-published")?;
            metadata.complete_repository_operation();
            metadata
                .replace_prepared(workspace.root)?
                .publish()
                .map_err(|error| anyhow::anyhow!(error))?;
            workspace_registry_service::reconcile(workspace.root, workspace.store_root, metadata)?;
            Some(head)
        }
        None => None,
    };
    let rebound = head.is_some();
    let head = match head {
        Some(head) => head,
        None => runtime.block_on(publisher.publish_envelope(expected, root))?,
    };
    let mut human = format!("moved label {name} from view {from} to view {to}\n");
    if rebound {
        writeln!(
            human,
            "this workspace now follows label {name} at view {to}"
        )?;
    }
    Success::new(
        "label.move",
        serde_json::json!({
            "schema_version": 1,
            "head": HeadDocument::from(head),
            "label": entry,
            "moved": true,
            "from": from.to_string(),
            "rebound": rebound,
        }),
        human,
    )
}

fn ensure_expected(expected: RepositoryHead, observed: RepositoryHead) -> Result<()> {
    if observed != expected {
        return Err(CliFailure::stale(format!(
            "repository head changed: expected generation {} envelope {}, found generation {} envelope {}",
            expected.generation, expected.envelope, observed.generation, observed.envelope
        )));
    }
    Ok(())
}

async fn load(repository: &LocalRepository) -> Result<LabelList> {
    let head = repository.heads.read()?;
    let reader = RepositoryReader::new(&repository.objects, &repository.content);
    let envelope = reader.envelope(head.envelope).await?;
    let manifest = reader.view_label_manifest(envelope.labels).await?;
    let labels = manifest
        .labels
        .iter()
        .map(|(name, label)| LabelEntry::new(name.clone(), label))
        .collect::<Result<_>>()?;
    Ok(LabelList {
        schema_version: 1,
        head: head.into(),
        labels,
    })
}

fn mutate<F>(
    repository: &LocalRepository,
    expected: Option<RepositoryHead>,
    command: &'static str,
    change: F,
) -> Result<Success>
where
    F: FnOnce(&mut std::collections::BTreeMap<String, ViewLabel>) -> Result<LabelEntry>,
{
    runtime()?.block_on(async {
        let observed = repository.heads.read()?;
        let expected = expected.unwrap_or(observed);
        ensure_expected(expected, observed)?;
        let reader = RepositoryReader::new(&repository.objects, &repository.content);
        let envelope = reader.envelope(expected.envelope).await?;
        let manifest = reader.view_label_manifest(envelope.labels).await?;
        let mut labels = manifest.labels;
        let entry = change(&mut labels)?;
        let manifest =
            ViewLabelManifestRecord::new(labels).context("invalid native label manifest")?;
        let publisher =
            RepositoryPublisher::new(&repository.objects, &repository.content, &repository.heads);
        // Publishing validates what the new manifest changes: each created or moved
        // label must name a retained view.
        let root = publisher.stage_envelope(envelope.state, &manifest).await?;
        let head = publisher.publish_envelope(expected, root).await?;
        Success::new(
            command,
            serde_json::json!({
                "schema_version": 1,
                "head": HeadDocument::from(head),
                "label": entry,
            }),
            format!("{}\t{}\n", entry.name, entry.view),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expected_head_parser_requires_both_full_fields() {
        let envelope = RepositoryEnvelopeId::from_digest([9; 32]);
        let parsed = parse_expected_head(&format!("17:{envelope}")).unwrap();
        assert_eq!(parsed.generation, 17);
        assert_eq!(parsed.envelope, envelope);
        assert!(parse_expected_head("17").is_err());
        assert!(parse_expected_head(&format!("x:{envelope}")).is_err());
        assert!(parse_expected_head("17:not-an-id").is_err());
    }
}
