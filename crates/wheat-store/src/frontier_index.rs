//! A local cache of the views below the views labels point at, so a label's
//! descendant frontiers cost what they number instead of a walk over the history
//! under the label.
//!
//! For each view it indexes (an *anchor*) the index holds every view reachable from
//! it, each with whether it is a tip, valid for the lineage root in `stamp`. A query
//! against a later lineage first applies the edges that lineage adds
//! ([`ViewLineageMapStore::growth`], which costs what the two differ by): a parent
//! below an anchor, or the anchor itself, puts its new children and everything below
//! them below the anchor too. A lineage that drops edges (a rewrite), or one the
//! stamp cannot be compared with, empties the index. An anchor the index does not
//! hold yet is built with the forward walk, once. Anchors no label points at any more
//! are dropped.
//!
//! The lineage is the authority; the index can be deleted at any time. Every write
//! checks and bumps `stamp.generation`, so a process that read the index before
//! another one changed it starts over instead of writing over it.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::Path,
    sync::{Mutex, MutexGuard, PoisonError},
};

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use thiserror::Error;
use wheat_core::{ViewId, ViewLineageMapId};

use crate::{
    ImmutableObjectStore, ViewLineageError, ViewLineageLimits, ViewLineageMapStore,
    view_lineage::LineageGrowth,
};

const SCHEMA_VERSION: i64 = 1;
const SCHEMA: &str = include_str!("frontier_index_sql/schema.sql");
const DROP: &str = include_str!("frontier_index_sql/drop.sql");
const CLEAR: &str = include_str!("frontier_index_sql/clear.sql");
const STAMP: &str = include_str!("frontier_index_sql/stamp.sql");
const SET_STAMP: &str = include_str!("frontier_index_sql/set_stamp.sql");
const ANCHORS: &str = include_str!("frontier_index_sql/anchors.sql");
const ANCHOR_DESCENDANTS: &str = include_str!("frontier_index_sql/anchor_descendants.sql");
const ANCHORS_ABOVE: &str = include_str!("frontier_index_sql/anchors_above.sql");
const IS_BELOW: &str = include_str!("frontier_index_sql/is_below.sql");
const TIPS: &str = include_str!("frontier_index_sql/tips.sql");
const CLEAR_TIP: &str = include_str!("frontier_index_sql/clear_tip.sql");
const INSERT_DESCENDANT: &str = include_str!("frontier_index_sql/insert_descendant.sql");
const INSERT_ANCHOR: &str = include_str!("frontier_index_sql/insert_anchor.sql");
const COUNT_DESCENDANT: &str = include_str!("frontier_index_sql/count_descendant.sql");
const DELETE_ANCHOR: &str = include_str!("frontier_index_sql/delete_anchor.sql");
const DELETE_ANCHOR_DESCENDANTS: &str =
    include_str!("frontier_index_sql/delete_anchor_descendants.sql");

/// How many times a query starts over when other processes change the index under it
/// before it answers with the walk instead.
const ATTEMPTS: usize = 4;

/// How long a write waits for another process's write to finish.
const BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

#[derive(Debug, Error)]
pub enum FrontierIndexError {
    #[error("frontier index file failed")]
    File(#[from] std::io::Error),
    #[error("frontier index database failed")]
    Database(#[from] rusqlite::Error),
    #[error("frontier index holds a malformed view ID")]
    Malformed,
    #[error(transparent)]
    Lineage(#[from] ViewLineageError),
}

/// An anchor's descendant count and sorted tips.
type Anchored = (usize, Vec<ViewId>);

/// What [`FrontierIndex`] read, or why it started over.
enum Step<T> {
    Done(T),
    /// Another process changed the index since this one read its generation.
    Raced,
}

/// The descendants of the views labels point at, kept in a SQLite file next to the
/// store. See the module documentation.
///
/// The connection sits behind a mutex so a query future holding `&self` across an
/// await is `Send`; it is never held across one.
pub struct FrontierIndex {
    connection: Mutex<Connection>,
}

impl FrontierIndex {
    /// Opens the index at `path`, creating it when missing. A file this version cannot
    /// read is emptied: the index holds nothing the lineage does not.
    pub fn open(path: &Path) -> Result<Self, FrontierIndexError> {
        let connection = match Self::connect(path) {
            Ok(connection) => connection,
            Err(FrontierIndexError::Database(_)) => {
                for suffix in ["", "-wal", "-shm"] {
                    let mut file = path.as_os_str().to_owned();
                    file.push(suffix);
                    match std::fs::remove_file(&file) {
                        Ok(()) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error.into()),
                    }
                }
                Self::connect(path)?
            }
            Err(error) => return Err(error),
        };
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    fn connect(path: &Path) -> Result<Connection, FrontierIndexError> {
        let connection = Connection::open(path)?;
        connection.busy_timeout(BUSY_TIMEOUT)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "NORMAL")?;
        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version != SCHEMA_VERSION {
            connection.execute_batch(DROP)?;
            connection.execute_batch(SCHEMA)?;
            connection.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }
        Ok(connection)
    }

    /// The connection. A panic mid-statement leaves an open transaction that rolls
    /// back, so a poisoned lock is taken as is.
    fn connection(&self) -> MutexGuard<'_, Connection> {
        self.connection
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// `basis`'s descendant frontiers in `lineage`: what
    /// [`ViewLineageMapStore::descendant_frontiers`] returns, under the same limits.
    /// `labels` are the views the repository's labels point at; the index keeps
    /// those and `basis`, and drops the others it holds.
    #[tracing::instrument(name = "frontier_index", level = "debug", skip_all)]
    pub async fn frontiers<S: ImmutableObjectStore>(
        &self,
        objects: &S,
        lineage: ViewLineageMapId,
        labels: impl IntoIterator<Item = ViewId>,
        basis: ViewId,
        limits: ViewLineageLimits,
    ) -> Result<Vec<ViewId>, FrontierIndexError> {
        let store = ViewLineageMapStore::new(objects);
        let mut keep = labels.into_iter().collect::<BTreeSet<_>>();
        keep.insert(basis);
        for _ in 0..ATTEMPTS {
            if let Step::Done(frontiers) =
                self.attempt(&store, lineage, &keep, basis, limits).await?
            {
                return Ok(frontiers);
            }
        }
        Ok(store.descendant_frontiers(lineage, basis, limits).await?)
    }

    async fn attempt<S: ImmutableObjectStore>(
        &self,
        store: &ViewLineageMapStore<'_, S>,
        lineage: ViewLineageMapId,
        keep: &BTreeSet<ViewId>,
        basis: ViewId,
        limits: ViewLineageLimits,
    ) -> Result<Step<Vec<ViewId>>, FrontierIndexError> {
        let Step::Done(generation) = self.catch_up(store, lineage, limits).await? else {
            return Ok(Step::Raced);
        };
        let Step::Done(generation) = self.evict(generation, keep)? else {
            return Ok(Step::Raced);
        };
        match self.tips(generation, basis)? {
            Step::Raced => Ok(Step::Raced),
            Step::Done(Some((descendants, tips))) => {
                check_limits(descendants, tips.len(), limits)?;
                Ok(Step::Done(tips))
            }
            Step::Done(None) => {
                let descendants = store.descendants(lineage, basis, limits).await?;
                let mut tips = descendants
                    .iter()
                    .filter_map(|(view, tip)| tip.then_some(*view))
                    .collect::<Vec<_>>();
                check_limits(descendants.len(), tips.len(), limits)?;
                let Step::Done(()) = self.insert_anchor(generation, basis, &descendants)? else {
                    return Ok(Step::Raced);
                };
                tips.sort_unstable();
                Ok(Step::Done(tips))
            }
        }
    }

    /// Brings the index to `lineage` and returns its generation there.
    async fn catch_up<S: ImmutableObjectStore>(
        &self,
        store: &ViewLineageMapStore<'_, S>,
        lineage: ViewLineageMapId,
        limits: ViewLineageLimits,
    ) -> Result<Step<i64>, FrontierIndexError> {
        let Some((at, generation)) = self.stamp()? else {
            return self.reset(None, lineage);
        };
        if at == lineage {
            return Ok(Step::Done(generation));
        }
        // A stamp whose lineage this store cannot read any more (collected after a
        // rewrite) is as good as a dropped one.
        let edges = match store.growth(at, lineage).await {
            Ok(LineageGrowth::Grew(edges)) => edges,
            Ok(LineageGrowth::Dropped) | Err(_) => return self.reset(Some(generation), lineage),
        };
        let rows = self.additions(store, lineage, &edges, limits).await?;
        self.apply(generation, lineage, &edges, &rows)
    }

    fn stamp(&self) -> Result<Option<(ViewLineageMapId, i64)>, FrontierIndexError> {
        let connection = self.connection();
        let row = connection
            .prepare_cached(STAMP)?
            .query_row([], |row| {
                Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, i64>(1)?))
            })
            .optional()?;
        row.map(|(lineage, generation)| {
            Ok((ViewLineageMapId::from_digest(digest(&lineage)?), generation))
        })
        .transpose()
    }

    /// Empties the index and stamps it with `lineage`, unless another process moved
    /// it on from `generation` (none: the index had no stamp).
    fn reset(
        &self,
        generation: Option<i64>,
        lineage: ViewLineageMapId,
    ) -> Result<Step<i64>, FrontierIndexError> {
        self.write(generation, |transaction| {
            transaction.execute_batch(CLEAR)?;
            Ok(lineage)
        })
    }

    /// The views each new edge puts below an anchor: for every anchor a parent is
    /// below or is, the parent's new children and whatever below them the anchor does
    /// not hold yet. Each row is an anchor, a view, and whether the view is a tip in
    /// `lineage`.
    async fn additions<S: ImmutableObjectStore>(
        &self,
        store: &ViewLineageMapStore<'_, S>,
        lineage: ViewLineageMapId,
        edges: &BTreeMap<ViewId, Vec<ViewId>>,
        limits: ViewLineageLimits,
    ) -> Result<Vec<(ViewId, ViewId, bool)>, FrontierIndexError> {
        let mut seeds = BTreeMap::<ViewId, Vec<ViewId>>::new();
        {
            let connection = self.connection();
            let anchors = read_views(&connection, ANCHORS, [])?
                .into_iter()
                .collect::<BTreeSet<_>>();
            for (parent, children) in edges {
                if children.is_empty() {
                    continue;
                }
                let mut above = read_views(&connection, ANCHORS_ABOVE, [parent.as_bytes()])?;
                if anchors.contains(parent) {
                    above.push(*parent);
                }
                for anchor in above {
                    seeds.entry(anchor).or_default().extend(children);
                }
            }
        }
        let mut children_of = HashMap::<ViewId, Vec<ViewId>>::new();
        let mut rows = Vec::new();
        let mut added = BTreeSet::new();
        for (anchor, mut pending) in seeds {
            added.clear();
            while let Some(view) = pending.pop() {
                if view == anchor || added.contains(&view) || self.is_below(anchor, view)? {
                    continue;
                }
                let children = match children_of.entry(view) {
                    std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
                    std::collections::hash_map::Entry::Vacant(entry) => {
                        entry.insert(store.children_of(lineage, view, limits).await?)
                    }
                };
                added.insert(view);
                rows.push((anchor, view, children.is_empty()));
                pending.extend(children.iter().copied());
            }
        }
        Ok(rows)
    }

    fn is_below(&self, anchor: ViewId, view: ViewId) -> Result<bool, FrontierIndexError> {
        Ok(self
            .connection()
            .prepare_cached(IS_BELOW)?
            .exists(params![anchor.as_bytes(), view.as_bytes()])?)
    }

    /// Writes what [`Self::additions`] found and stamps the index with `lineage`.
    fn apply(
        &self,
        generation: i64,
        lineage: ViewLineageMapId,
        edges: &BTreeMap<ViewId, Vec<ViewId>>,
        rows: &[(ViewId, ViewId, bool)],
    ) -> Result<Step<i64>, FrontierIndexError> {
        self.write(Some(generation), |transaction| {
            let mut clear_tip = transaction.prepare_cached(CLEAR_TIP)?;
            for (parent, children) in edges {
                if !children.is_empty() {
                    clear_tip.execute([parent.as_bytes()])?;
                }
            }
            let mut insert = transaction.prepare_cached(INSERT_DESCENDANT)?;
            let mut count = transaction.prepare_cached(COUNT_DESCENDANT)?;
            for (anchor, view, tip) in rows {
                if insert.execute(params![anchor.as_bytes(), view.as_bytes(), tip])? == 1 {
                    count.execute([anchor.as_bytes()])?;
                }
            }
            Ok(lineage)
        })
    }

    /// Drops the anchors outside `keep`.
    fn evict(
        &self,
        generation: i64,
        keep: &BTreeSet<ViewId>,
    ) -> Result<Step<i64>, FrontierIndexError> {
        let stale = read_views(&self.connection(), ANCHORS, [])?
            .into_iter()
            .filter(|anchor| !keep.contains(anchor))
            .collect::<Vec<_>>();
        if stale.is_empty() {
            return Ok(Step::Done(generation));
        }
        let lineage = self.stamp()?.map(|(lineage, _)| lineage);
        let Some(lineage) = lineage else {
            return Ok(Step::Raced);
        };
        self.write(Some(generation), |transaction| {
            let mut descendants = transaction.prepare_cached(DELETE_ANCHOR_DESCENDANTS)?;
            let mut anchors = transaction.prepare_cached(DELETE_ANCHOR)?;
            for anchor in &stale {
                descendants.execute([anchor.as_bytes()])?;
                anchors.execute([anchor.as_bytes()])?;
            }
            Ok(lineage)
        })
    }

    /// `basis`'s descendant count and sorted tips, none when it is not an anchor.
    fn tips(
        &self,
        generation: i64,
        basis: ViewId,
    ) -> Result<Step<Option<Anchored>>, FrontierIndexError> {
        let mut connection = self.connection();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Deferred)?;
        if current_generation(&transaction)? != Some(generation) {
            return Ok(Step::Raced);
        }
        let descendants = transaction
            .prepare_cached(ANCHOR_DESCENDANTS)?
            .query_row([basis.as_bytes()], |row| row.get::<_, i64>(0))
            .optional()?;
        let Some(descendants) = descendants else {
            return Ok(Step::Done(None));
        };
        let tips = read_views(&transaction, TIPS, [basis.as_bytes()])?;
        transaction.commit()?;
        let descendants =
            usize::try_from(descendants).map_err(|_| FrontierIndexError::Malformed)?;
        Ok(Step::Done(Some((descendants, tips))))
    }

    fn insert_anchor(
        &self,
        generation: i64,
        anchor: ViewId,
        descendants: &[(ViewId, bool)],
    ) -> Result<Step<()>, FrontierIndexError> {
        let Some((lineage, _)) = self.stamp()? else {
            return Ok(Step::Raced);
        };
        let count = i64::try_from(descendants.len()).map_err(|_| FrontierIndexError::Malformed)?;
        Ok(
            match self.write(Some(generation), |transaction| {
                transaction.execute(DELETE_ANCHOR_DESCENDANTS, [anchor.as_bytes()])?;
                transaction.execute(INSERT_ANCHOR, params![anchor.as_bytes(), count])?;
                let mut insert = transaction.prepare_cached(INSERT_DESCENDANT)?;
                for (view, tip) in descendants {
                    insert.execute(params![anchor.as_bytes(), view.as_bytes(), tip])?;
                }
                Ok(lineage)
            })? {
                Step::Done(_) => Step::Done(()),
                Step::Raced => Step::Raced,
            },
        )
    }

    /// Runs `change` in one write transaction if the index is still at `generation`
    /// (none: it has no stamp), then stamps it with the lineage `change` returns and
    /// the next generation, which it returns.
    fn write(
        &self,
        generation: Option<i64>,
        change: impl FnOnce(&Transaction<'_>) -> Result<ViewLineageMapId, FrontierIndexError>,
    ) -> Result<Step<i64>, FrontierIndexError> {
        let mut connection = self.connection();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if current_generation(&transaction)? != generation {
            return Ok(Step::Raced);
        }
        let lineage = change(&transaction)?;
        let next = generation.map_or(0, |generation| generation + 1);
        transaction
            .prepare_cached(SET_STAMP)?
            .execute(params![lineage.as_bytes(), next])?;
        transaction.commit()?;
        Ok(Step::Done(next))
    }
}

fn current_generation(transaction: &Transaction<'_>) -> Result<Option<i64>, FrontierIndexError> {
    Ok(transaction
        .prepare_cached(STAMP)?
        .query_row([], |row| row.get::<_, i64>(1))
        .optional()?)
}

fn read_views<P: rusqlite::Params>(
    connection: &Connection,
    sql: &str,
    params: P,
) -> Result<Vec<ViewId>, FrontierIndexError> {
    let mut statement = connection.prepare_cached(sql)?;
    let mut rows = statement.query(params)?;
    let mut views = Vec::new();
    while let Some(row) = rows.next()? {
        views.push(ViewId::from_digest(digest(&row.get::<_, Vec<u8>>(0)?)?));
    }
    Ok(views)
}

fn digest(bytes: &[u8]) -> Result<[u8; 32], FrontierIndexError> {
    bytes.try_into().map_err(|_| FrontierIndexError::Malformed)
}

/// The walk's limits, held against what the index answers from: the walk visits
/// `basis` and its descendants, and refuses more tips than `max_frontiers`.
fn check_limits(
    descendants: usize,
    tips: usize,
    limits: ViewLineageLimits,
) -> Result<(), ViewLineageError> {
    if descendants + 1 > limits.max_reachable_views {
        return Err(ViewLineageError::ReachableViewLimitExceeded {
            limit: limits.max_reachable_views,
        });
    }
    if tips > limits.max_frontiers {
        return Err(ViewLineageError::FrontierLimitExceeded {
            limit: limits.max_frontiers,
        });
    }
    Ok(())
}
