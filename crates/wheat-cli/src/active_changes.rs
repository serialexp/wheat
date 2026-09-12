use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use wheat_core::{ChangeId, RevisionId, ViewId};
use wheat_store::{
    ApplicationMapStore, ChangeMapStore, RepositoryHead, RepositoryReader, RevisionMapStore,
};

use crate::local::LocalRepository;

const MAX_ACTIVE_CHANGES: usize = 250_000;
const MAX_TITLE_BYTES: usize = 1024 * 1024;
const MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ActiveChange {
    #[serde(serialize_with = "display_id")]
    pub change: ChangeId,
    #[serde(serialize_with = "display_id")]
    pub revision: RevisionId,
    pub title: String,
    #[serde(serialize_with = "display_ids")]
    pub dependencies: Vec<ChangeId>,
    pub conflicted: bool,
    #[serde(serialize_with = "display_optional_id")]
    pub published_revision: Option<RevisionId>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ActiveChanges {
    pub format: &'static str,
    pub version: u32,
    #[serde(serialize_with = "display_id")]
    pub basis: ViewId,
    #[serde(serialize_with = "display_id")]
    pub working: ViewId,
    #[serde(serialize_with = "display_id")]
    pub published: ViewId,
    #[serde(serialize_with = "display_optional_id")]
    pub selected: Option<ChangeId>,
    pub changes: Vec<ActiveChange>,
}

pub(crate) fn render_json(document: &ActiveChanges) -> Result<Vec<u8>> {
    let bytes = serde_json::to_vec_pretty(document)?;
    anyhow::ensure!(
        bytes.len() <= MAX_OUTPUT_BYTES,
        "active-change JSON exceeds the output limit"
    );
    Ok(bytes)
}

pub(crate) fn render_human(document: &ActiveChanges) -> Result<Vec<u8>> {
    use std::fmt::Write as _;
    let mut output = String::new();
    for change in &document.changes {
        let marker = if Some(change.change) == document.selected {
            "*"
        } else {
            " "
        };
        let conflict = if change.conflicted { " conflicted" } else { "" };
        writeln!(
            output,
            "{marker} {} {}{conflict}",
            change.change, change.title
        )?;
        writeln!(output, "    revision {}", change.revision)?;
        anyhow::ensure!(
            output.len() <= MAX_OUTPUT_BYTES,
            "active-change output exceeds the limit"
        );
    }
    Ok(output.into_bytes())
}

fn display_id<T: std::fmt::Display, S: serde::Serializer>(
    value: &T,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    serializer.collect_str(value)
}

fn display_optional_id<T: std::fmt::Display, S: serde::Serializer>(
    value: &Option<T>,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    match value {
        Some(value) => serializer.collect_str(value),
        None => serializer.serialize_none(),
    }
}

fn display_ids<T: std::fmt::Display, S: serde::Serializer>(
    values: &[T],
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    values
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .serialize(serializer)
}

pub(crate) async fn inspect(
    repository: &LocalRepository,
    head: RepositoryHead,
    basis: ViewId,
    selected: Option<ChangeId>,
) -> Result<ActiveChanges> {
    let reader = RepositoryReader::new(&repository.objects, &repository.content);
    let envelope = reader.envelope(head.envelope).await?;
    let state = reader.state(envelope.state).await?;
    let basis_view = reader
        .view(basis)
        .await
        .context("authoring basis is not retained")?;
    let working = reader.view(state.working_view).await?;
    let published = reader.view(state.published_view).await?;
    let revisions = RevisionMapStore::new(&repository.objects);
    let applications = ApplicationMapStore::new(&repository.objects);
    let changes = ChangeMapStore::new(&repository.objects);
    let basis_revisions = collect_revisions(&revisions, basis_view.revisions).await?;
    let published_revisions = collect_revisions(&revisions, published.revisions).await?;
    let mut cursor = revisions.cursor(working.revisions).await?;
    let mut active = BTreeMap::new();
    while let Some((change, revision)) = cursor.next().await? {
        if basis_revisions.get(&change) == Some(&revision) {
            continue;
        }
        if active.len() == MAX_ACTIVE_CHANGES {
            bail!("active change count exceeds the command limit");
        }
        let record = reader.revision(revision).await?;
        if record.change_id != change {
            bail!("working revision-map change identity mismatch");
        }
        let object = changes
            .get(state.changes, change)
            .await?
            .context("active revision refers to an unindexed logical change")?;
        let change_record = reader.change(object).await?;
        if change_record.id != change {
            bail!("change-map key does not match logical change identity");
        }
        if change_record.metadata.title.len() > MAX_TITLE_BYTES {
            bail!("active change title exceeds the command limit");
        }
        let application = applications
            .get(working.applications, change)
            .await?
            .context("active change has no working application")?;
        let application = reader.application(application).await?;
        let conflicted = matches!(
            application.result,
            wheat_core::ApplicationResultRecord::Conflicted { .. }
        );
        active.insert(
            change,
            ActiveChange {
                change,
                revision,
                title: change_record.metadata.title,
                dependencies: record.dependencies.iter().copied().collect(),
                conflicted,
                published_revision: published_revisions.get(&change).copied(),
            },
        );
    }
    let ordered = topological(active)?;
    Ok(ActiveChanges {
        format: "wheat-active-changes",
        version: 1,
        basis,
        working: state.working_view,
        published: state.published_view,
        selected,
        changes: ordered,
    })
}

async fn collect_revisions<S: wheat_store::ImmutableObjectStore>(
    revisions: &RevisionMapStore<'_, S>,
    root: wheat_core::RevisionMapId,
) -> Result<BTreeMap<ChangeId, RevisionId>> {
    let mut values = BTreeMap::new();
    let mut cursor = revisions.cursor(root).await?;
    while let Some((change, revision)) = cursor.next().await? {
        if values.len() == MAX_ACTIVE_CHANGES {
            bail!("revision count exceeds the active-change command limit");
        }
        values.insert(change, revision);
    }
    Ok(values)
}

fn topological(mut changes: BTreeMap<ChangeId, ActiveChange>) -> Result<Vec<ActiveChange>> {
    let selected = changes.keys().copied().collect::<BTreeSet<_>>();
    let mut remaining = changes
        .iter()
        .map(|(id, change)| {
            (
                *id,
                change
                    .dependencies
                    .iter()
                    .filter(|id| selected.contains(id))
                    .count(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut reverse = BTreeMap::<ChangeId, Vec<ChangeId>>::new();
    for (id, change) in &changes {
        for dependency in change
            .dependencies
            .iter()
            .filter(|id| selected.contains(id))
        {
            reverse.entry(*dependency).or_default().push(*id);
        }
    }
    let mut ready = remaining
        .iter()
        .filter_map(|(id, n)| (*n == 0).then_some(*id))
        .collect::<BTreeSet<_>>();
    let mut ordered = Vec::with_capacity(changes.len());
    while let Some(id) = ready.pop_first() {
        ordered.push(changes.remove(&id).expect("ready change exists"));
        if let Some(children) = reverse.get(&id) {
            for child in children {
                let count = remaining.get_mut(child).expect("active child exists");
                *count -= 1;
                if *count == 0 {
                    ready.insert(*child);
                }
            }
        }
    }
    if !changes.is_empty() {
        bail!("active change dependencies contain a cycle");
    }
    Ok(ordered)
}
