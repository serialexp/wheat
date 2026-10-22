//! Native short-label inspection and compare-and-swap mutation.

use std::{fmt::Write as _, str::FromStr};

use anyhow::{Context, Result};
use serde::Serialize;
use wheat_core::{
    LabelAnnotation, MAX_LABELS, RepositoryEnvelopeId, ViewId, ViewLabel, ViewLabelManifestRecord,
};
use wheat_store::{PublicationStore, RepositoryHead, RepositoryPublisher, RepositoryReader};

use crate::{
    authoring_metadata::display_timestamp,
    local::{LocalRepository, runtime},
    outcome::{CliFailure, Success},
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

async fn load(repository: &LocalRepository) -> Result<LabelList> {
    let head = repository.heads.read()?;
    let reader = RepositoryReader::new(&repository.objects, &repository.content);
    let envelope = reader.validate_envelope(head.envelope).await?;
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
        if observed != expected {
            return Err(CliFailure::stale(format!(
                "repository head changed: expected generation {} envelope {}, found generation {} envelope {}",
                expected.generation, expected.envelope, observed.generation, observed.envelope
            )));
        }
        let reader = RepositoryReader::new(&repository.objects, &repository.content);
        let envelope = reader.validate_envelope(expected.envelope).await?;
        let manifest = reader.view_label_manifest(envelope.labels).await?;
        let mut labels = manifest.labels;
        let entry = change(&mut labels)?;
        let manifest = ViewLabelManifestRecord::new(labels)
            .context("invalid native label manifest")?;
        let publisher = RepositoryPublisher::new(
            &repository.objects,
            &repository.content,
            &repository.heads,
        );
        let root = publisher.prepare_envelope(envelope.state, &manifest).await?;
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
