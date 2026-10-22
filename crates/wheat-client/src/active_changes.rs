use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use serde::Serialize;
use wheat_core::{ChangeId, RevisionId, ViewId};
use wheat_store::{
    ApplicationMapStore, ChangeMapStore, ObjectStoreError, RepositoryHead,
    RepositoryPublicationError, RepositoryReader, RevisionMapStore,
};

use crate::{
    local::LocalRepository,
    outcome::{CliFailure, display_id, display_ids, display_optional_id},
};

const MAX_ACTIVE_CHANGES: usize = 250_000;
const MAX_TITLE_BYTES: usize = 1024 * 1024;
const MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug, Serialize)]
pub struct ActiveChange {
    #[serde(serialize_with = "display_id")]
    pub change: ChangeId,
    #[serde(serialize_with = "display_id")]
    pub revision: RevisionId,
    pub title: String,
    #[serde(serialize_with = "display_ids")]
    pub dependencies: Vec<ChangeId>,
    pub conflicted: bool,
    #[serde(serialize_with = "display_optional_id")]
    pub basis_revision: Option<RevisionId>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ActiveChanges {
    pub format: &'static str,
    pub version: u32,
    #[serde(serialize_with = "display_id")]
    pub basis: ViewId,
    #[serde(serialize_with = "display_id")]
    pub effective: ViewId,
    #[serde(serialize_with = "display_optional_id")]
    pub selected: Option<ChangeId>,
    pub changes: Vec<ActiveChange>,
}

pub fn render_human(document: &ActiveChanges) -> Result<Vec<u8>> {
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
        if output.len() > MAX_OUTPUT_BYTES {
            return Err(CliFailure::limit("active-change output exceeds the limit"));
        }
    }
    Ok(output.into_bytes())
}

#[tracing::instrument(name = "active_changes", level = "debug", skip_all)]
pub async fn inspect(
    repository: &LocalRepository,
    head: RepositoryHead,
    basis: ViewId,
    effective: ViewId,
    selected: Option<ChangeId>,
) -> Result<ActiveChanges> {
    let reader = RepositoryReader::new(&repository.objects, &repository.content);
    let envelope = reader.envelope(head.envelope).await?;
    let state = reader.state(envelope.state).await?;
    let basis_view = match reader.view(basis).await {
        Ok(view) => view,
        Err(RepositoryPublicationError::Object(ObjectStoreError::Missing(_))) => {
            return Err(CliFailure::corrupt("authoring basis is not retained"));
        }
        Err(error) => return Err(error.into()),
    };
    let working = reader.view(effective).await?;
    let revisions = RevisionMapStore::new(&repository.objects);
    let applications = ApplicationMapStore::new(&repository.objects);
    let changes = ChangeMapStore::new(&repository.objects);
    let basis_revisions = collect_revisions(&revisions, basis_view.revisions).await?;
    let mut cursor = revisions.cursor(working.revisions).await?;
    let mut active = BTreeMap::new();
    while let Some((change, revision)) = cursor.next().await? {
        if basis_revisions.get(&change) == Some(&revision) {
            continue;
        }
        if active.len() == MAX_ACTIVE_CHANGES {
            return Err(CliFailure::limit(
                "active change count exceeds the command limit",
            ));
        }
        let record = reader.revision(revision).await?;
        if record.change_id != change {
            return Err(CliFailure::corrupt(
                "working revision-map change identity mismatch",
            ));
        }
        let object = changes.get(state.changes, change).await?.ok_or_else(|| {
            CliFailure::corrupt("active revision refers to an unindexed logical change")
        })?;
        let change_record = reader.change(object).await?;
        if change_record.id != change {
            return Err(CliFailure::corrupt(
                "change-map key does not match logical change identity",
            ));
        }
        if change_record.metadata.title.len() > MAX_TITLE_BYTES {
            return Err(CliFailure::limit(
                "active change title exceeds the command limit",
            ));
        }
        let application = applications
            .get(working.applications, change)
            .await?
            .ok_or_else(|| CliFailure::corrupt("active change has no working application"))?;
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
                basis_revision: basis_revisions.get(&change).copied(),
            },
        );
    }
    let ordered = topological(active)?;
    Ok(ActiveChanges {
        format: "wheat-active-changes",
        version: 1,
        basis,
        effective,
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
            return Err(CliFailure::limit(
                "revision count exceeds the active-change command limit",
            ));
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
        return Err(CliFailure::corrupt(
            "active change dependencies contain a cycle",
        ));
    }
    Ok(ordered)
}
