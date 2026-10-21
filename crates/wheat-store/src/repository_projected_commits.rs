//! Recording the Git commit an application projects as.
//!
//! Exporting an application to Git is deterministic: the same application gives the
//! same commit on any machine, at any time. `wheat push` records each commit it
//! projects in the repository state before pushing it, so a later `wheat pull` in
//! this or any store the state reaches recognises the commit as that application
//! instead of importing it again. A member says only how the application projects,
//! not that any remote holds it.

use wheat_core::{ProjectedCommit, RepositoryStateRecord, ViewId};

use crate::{
    ImmutableObjectStore, ProjectedCommitSetStore, PublicationStore, RepositoryHead,
    RepositoryPublicationError, RepositoryReader, RetainedSetStore,
    repository_mutation::{
        PersistedRepositoryMutator, PreparedRepositoryMutation, RepositoryMutationError,
    },
};

type Result<T> = std::result::Result<T, RepositoryMutationError>;

impl<O: ImmutableObjectStore, P: PublicationStore> PersistedRepositoryMutator<'_, O, P> {
    /// Adds `members` to the state at `expected`, leaving every view as it is: `view` is
    /// the one the caller stays on. `None` when the state holds them all already.
    ///
    /// Each member's application must be retained, which keeps the state valid.
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn record_projected_commits(
        &self,
        expected: RepositoryHead,
        view: ViewId,
        members: impl IntoIterator<Item = ProjectedCommit>,
    ) -> Result<Option<PreparedRepositoryMutation>> {
        let actual = self
            .publications
            .read()
            .map_err(RepositoryPublicationError::from)?;
        if actual != expected {
            return Err(RepositoryMutationError::Stale { expected, actual });
        }
        let reader = RepositoryReader::new(self.objects, self.content);
        let state = reader
            .state(reader.envelope(expected.envelope).await?.state)
            .await?;
        let retained = RetainedSetStore::new(self.objects);
        let projected = ProjectedCommitSetStore::new(self.objects);
        let mut added = Vec::new();
        for member in members {
            if !retained
                .contains(state.retained_applications, member.application().into())
                .await?
            {
                return Err(RepositoryPublicationError::InvalidReferences(
                    "projected commit names an application the state does not retain",
                )
                .into());
            }
            if !projected.contains(state.projected_commits, member).await? {
                added.push((member, true));
            }
        }
        if added.is_empty() {
            return Ok(None);
        }
        let state = RepositoryStateRecord {
            projected_commits: projected
                .update_batch(state.projected_commits, added)
                .await?,
            ..state
        };
        self.objects
            .put_if_absent(state.id().into(), &state.canonical_bytes())
            .await?;
        Ok(Some(PreparedRepositoryMutation::state_only(
            expected,
            state.id(),
            view,
        )))
    }
}
