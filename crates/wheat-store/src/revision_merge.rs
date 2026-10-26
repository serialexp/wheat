//! Merging revisions of one change over their ancestry.
//!
//! A revision's parents are its predecessor and the revisions it combines; its
//! ancestors are its parents, transitively. Two revisions merge over their merge base,
//! the best revisions both descend from: their maximal common ancestors, found as Git
//! finds them, walking down by generation from both with each revision marked by the
//! sides that reach it. Several bases (a criss-cross) merge into one virtual base
//! first, as Git's recursive strategy does.
//!
//! Over a base holding edits `E(b)`, an edit of the base is kept when both sides hold
//! it, and any other edit when a side holds it and the other side either holds it too
//! or removed it since the base. What a side removed since the base is what the
//! revisions it reaches and the other side does not dropped from their parents' edits.
//! So an edit one side's line removed stays removed even when another line added the
//! same edit on its own.
//!
//! More than two revisions merge two at a time, in date order: each step is a virtual
//! revision whose parents are its two inputs, so the next step finds its base through
//! them. Virtual revisions live for one merge and are never stored.
//!
//! The work follows the revisions between the inputs and their bases, never the
//! change's whole history unless the inputs share none of it, and is bounded by a
//! [`MergeBudget`]: over budget is an error naming the change, not a wrong merge. The
//! budget meters work, not the history's shape: a criss-cross of criss-crosses walks the
//! revisions it has already read again for each virtual base, and those walks count
//! steps however deep the bases nest.

use std::collections::{BTreeSet, HashMap, HashSet};

use wheat_core::{ChangeId, EditId, RevisionId, RevisionRecord};

use crate::{ImmutableObjectStore, RepositoryPublicationError, RepositoryReader};

type Result<T> = std::result::Result<T, RepositoryPublicationError>;

/// How much work merging one change's revisions may take. Integration authors a
/// combined revision within it and validation checks one within it, so a merge an
/// author could make is one a validator accepts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MergeBudget {
    /// Revision records read.
    pub max_revision_reads: u64,
    /// Revisions taken and parents followed by the walks down the ancestry, summed over
    /// every walk, including those over records already read.
    pub max_walk_steps: u64,
    /// Edits compared or collected, summed over every merge step.
    pub max_edits: u64,
}

impl MergeBudget {
    pub const STANDARD: Self = Self {
        max_revision_reads: 4096,
        max_walk_steps: 1 << 18,
        max_edits: 1 << 20,
    };

    /// Takes `work` off what is left, for a budget several merges share.
    pub fn spend(&mut self, work: MergeWork) {
        self.max_revision_reads = self.max_revision_reads.saturating_sub(work.revision_reads);
        self.max_walk_steps = self.max_walk_steps.saturating_sub(work.walk_steps);
        self.max_edits = self.max_edits.saturating_sub(work.edits);
    }
}

/// The work one merge took, metered as [`MergeBudget`] bounds it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MergeWork {
    pub revision_reads: u64,
    pub walk_steps: u64,
    pub edits: u64,
}

/// The order revisions merge in, and so which one a combined revision follows: by
/// commit date, ties broken by the lowest id.
pub(crate) fn merge_order(id: RevisionId, record: &RevisionRecord) -> (i64, RevisionId) {
    (record.metadata.committed_at.seconds, id)
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
enum Node {
    Stored(RevisionId),
    /// An index into [`RevisionMerge::virtuals`].
    Virtual(usize),
}

/// A merge step's result: a revision with two parents that exists only in memory.
struct VirtualRevision {
    parents: Vec<Node>,
    generation: u64,
    edits: BTreeSet<EditId>,
    /// The parents' edits it dropped.
    removed: BTreeSet<EditId>,
}

/// Which sides of a walk reached a revision, and whether it is below a common one.
const LEFT: u8 = 1;
const RIGHT: u8 = 2;
const STALE: u8 = 4;

/// What walking down from two revisions found.
struct Walk {
    /// Their maximal common ancestors, highest generation first.
    bases: Vec<Node>,
    /// The edits each side removed since the bases.
    removed: [BTreeSet<EditId>; 2],
}

/// One merge of `change`'s revisions, with the records it has read.
pub(crate) struct RevisionMerge<'r, 'a, O> {
    reader: &'r RepositoryReader<'a, O>,
    change: ChangeId,
    budget: MergeBudget,
    work: MergeWork,
    records: HashMap<RevisionId, RevisionRecord>,
    virtuals: Vec<VirtualRevision>,
    /// Merges already made, by their inputs, so a base shared by several steps is
    /// built once.
    merged: HashMap<(Node, Node), Node>,
}

impl<'r, 'a, O: ImmutableObjectStore> RevisionMerge<'r, 'a, O> {
    pub(crate) fn new(
        reader: &'r RepositoryReader<'a, O>,
        change: ChangeId,
        budget: MergeBudget,
    ) -> Self {
        Self {
            reader,
            change,
            budget,
            work: MergeWork::default(),
            records: HashMap::new(),
            virtuals: Vec::new(),
            merged: HashMap::new(),
        }
    }

    fn over_budget(&self, limit: &'static str) -> RepositoryPublicationError {
        RepositoryPublicationError::MergeOverBudget {
            change: self.change,
            limit,
        }
    }

    pub(crate) fn work(&self) -> MergeWork {
        self.work
    }

    /// Counts one step of a walk.
    fn step(&mut self) -> Result<()> {
        if self.work.walk_steps >= self.budget.max_walk_steps {
            return Err(self.over_budget("walk steps"));
        }
        self.work.walk_steps += 1;
        Ok(())
    }

    /// Counts `count` edits compared or collected.
    fn charge_edits(&mut self, count: usize) -> Result<()> {
        self.work.edits = self
            .work
            .edits
            .saturating_add(u64::try_from(count).unwrap_or(u64::MAX));
        if self.work.edits > self.budget.max_edits {
            return Err(self.over_budget("edits compared"));
        }
        Ok(())
    }

    /// `id`'s record, read once per merge.
    pub(crate) async fn record(&mut self, id: RevisionId) -> Result<&RevisionRecord> {
        if !self.records.contains_key(&id) {
            if self.work.revision_reads >= self.budget.max_revision_reads {
                return Err(self.over_budget("revision reads"));
            }
            self.work.revision_reads += 1;
            let record = self.reader.revision(id).await?;
            if record.change_id != self.change {
                return Err(RepositoryPublicationError::InvalidReferences(
                    "a revision's ancestry holds a revision of another change",
                ));
            }
            self.records.insert(id, record);
        }
        Ok(&self.records[&id])
    }

    async fn load(&mut self, node: Node) -> Result<()> {
        if let Node::Stored(id) = node {
            self.record(id).await?;
        }
        Ok(())
    }

    /// A loaded node's parents.
    fn parents(&self, node: Node) -> Vec<Node> {
        match node {
            Node::Stored(id) => {
                let record = &self.records[&id];
                record
                    .predecessor
                    .iter()
                    .chain(&record.combines)
                    .map(|parent| Node::Stored(*parent))
                    .collect()
            }
            Node::Virtual(index) => self.virtuals[index].parents.clone(),
        }
    }

    fn generation(&self, node: Node) -> u64 {
        match node {
            Node::Stored(id) => self.records[&id].generation,
            Node::Virtual(index) => self.virtuals[index].generation,
        }
    }

    fn edits(&self, node: Node) -> &BTreeSet<EditId> {
        match node {
            Node::Stored(id) => &self.records[&id].edits,
            Node::Virtual(index) => &self.virtuals[index].edits,
        }
    }

    /// The edits a loaded node dropped from its parents'. A plain revision's transition
    /// records them; a combined one's are its parents' edits it does not hold.
    async fn removed(&mut self, node: Node) -> Result<BTreeSet<EditId>> {
        match node {
            Node::Virtual(index) => {
                self.charge_edits(self.virtuals[index].removed.len())?;
                Ok(self.virtuals[index].removed.clone())
            }
            Node::Stored(id) => {
                let record = &self.records[&id];
                if record.combines.is_empty() {
                    let count = record.transition.removed.len();
                    self.charge_edits(count)?;
                    return Ok(self.records[&id].transition.removed.clone());
                }
                let parents = self.parents(node);
                let mut removed = BTreeSet::new();
                for parent in parents {
                    self.load(parent).await?;
                    self.charge_edits(self.edits(parent).len())?;
                    removed.extend(self.edits(parent).iter().copied());
                }
                self.charge_edits(self.edits(node).len())?;
                for edit in self.edits(node) {
                    removed.remove(edit);
                }
                Ok(removed)
            }
        }
    }

    /// Walks down from `left` and `right` by generation, marking each revision with the
    /// sides that reach it. A revision both reach is common; it and everything below it
    /// is stale. Processing by generation means a revision's marks are final when it is
    /// taken, so the common revisions found are exactly the maximal ones, and a revision
    /// one side alone reaches is one that side holds since the bases. The walk ends once
    /// only stale revisions are queued.
    async fn walk(&mut self, left: Node, right: Node) -> Result<Walk> {
        let mut marks: HashMap<Node, u8> = HashMap::new();
        let mut queue: BTreeSet<(u64, Node)> = BTreeSet::new();
        let mut live = 0usize;
        let mut walk = Walk {
            bases: Vec::new(),
            removed: [BTreeSet::new(), BTreeSet::new()],
        };
        for (node, side) in [(left, LEFT), (right, RIGHT)] {
            self.load(node).await?;
            let mark = marks.entry(node).or_default();
            *mark |= side;
            if queue.insert((self.generation(node), node)) {
                live += 1;
            }
        }
        while live > 0 {
            self.step()?;
            let (_, node) = queue.pop_last().expect("live revisions are queued");
            let mut mark = marks[&node];
            if mark & STALE == 0 {
                live -= 1;
            }
            if mark & STALE == 0 {
                if mark & (LEFT | RIGHT) == LEFT | RIGHT {
                    walk.bases.push(node);
                    mark |= STALE;
                } else {
                    let side = usize::from(mark & RIGHT != 0);
                    let removed = self.removed(node).await?;
                    walk.removed[side].extend(removed);
                }
            }
            for parent in self.parents(node) {
                self.step()?;
                self.load(parent).await?;
                let before = marks.get(&parent).copied().unwrap_or(0);
                let after = before | mark;
                if after == before {
                    continue;
                }
                marks.insert(parent, after);
                let key = (self.generation(parent), parent);
                if queue.insert(key) {
                    if after & STALE == 0 {
                        live += 1;
                    }
                } else if before & STALE == 0 && after & STALE != 0 {
                    live -= 1;
                }
            }
        }
        Ok(walk)
    }

    /// Merges two revisions over their merge base, as a virtual revision.
    async fn merge_two(&mut self, left: Node, right: Node) -> Result<Node> {
        if let Some(merged) = self.merged.get(&(left, right)) {
            return Ok(*merged);
        }
        let walk = self.walk(left, right).await?;
        let base = match walk.bases.as_slice() {
            [] => BTreeSet::new(),
            [base] => self.edits(*base).clone(),
            bases => {
                // Every nested merge walks at least its two inputs, so the walk-step
                // budget bounds how deep this goes.
                let mut bases = bases.to_vec();
                bases.sort();
                let mut virtual_base = bases[0];
                for next in &bases[1..] {
                    virtual_base = Box::pin(self.merge_two(virtual_base, *next)).await?;
                }
                self.edits(virtual_base).clone()
            }
        };
        self.charge_edits(self.edits(left).len() + self.edits(right).len() + base.len())?;
        let (left_edits, right_edits) = (self.edits(left), self.edits(right));
        let [left_removed, right_removed] = &walk.removed;
        let mut edits = BTreeSet::new();
        for edit in left_edits.union(right_edits) {
            let (in_left, in_right) = (left_edits.contains(edit), right_edits.contains(edit));
            let kept = if base.contains(edit) {
                in_left && in_right
            } else {
                (in_left && (in_right || !right_removed.contains(edit)))
                    || (in_right && (in_left || !left_removed.contains(edit)))
            };
            if kept {
                edits.insert(*edit);
            }
        }
        let removed: BTreeSet<EditId> = left_edits
            .union(right_edits)
            .filter(|edit| !edits.contains(*edit))
            .copied()
            .collect();
        let generation = self
            .generation(left)
            .max(self.generation(right))
            .checked_add(1)
            .ok_or(RepositoryPublicationError::InvalidReferences(
                "revision generation overflow",
            ))?;
        self.virtuals.push(VirtualRevision {
            parents: vec![left, right],
            generation,
            edits,
            removed,
        });
        let merged = Node::Virtual(self.virtuals.len() - 1);
        self.merged.insert((left, right), merged);
        Ok(merged)
    }

    /// The edits of the merge of `inputs`, two at a time in the order given (see
    /// [`merge_order`]). Each input must be loaded or readable.
    pub(crate) async fn merged_edits(&mut self, inputs: &[RevisionId]) -> Result<BTreeSet<EditId>> {
        let Some((first, rest)) = inputs.split_first() else {
            return Ok(BTreeSet::new());
        };
        let mut merged = Node::Stored(*first);
        self.load(merged).await?;
        for input in rest {
            let input = Node::Stored(*input);
            self.load(input).await?;
            merged = self.merge_two(merged, input).await?;
        }
        Ok(self.edits(merged).clone())
    }

    /// Those of `inputs` no other input descends from, in the order given. Walks down
    /// from all of them by generation, no lower than the lowest input: an input reached
    /// from another one is its ancestor.
    pub(crate) async fn maximal(&mut self, inputs: &[RevisionId]) -> Result<Vec<RevisionId>> {
        let mut queue: BTreeSet<(u64, RevisionId)> = BTreeSet::new();
        let mut reached: HashSet<RevisionId> = HashSet::new();
        let mut lowest = u64::MAX;
        for input in inputs {
            let generation = self.record(*input).await?.generation;
            lowest = lowest.min(generation);
            queue.insert((generation, *input));
        }
        while let Some((_, id)) = queue.pop_last() {
            self.step()?;
            let record = self.record(id).await?;
            let parents = record
                .predecessor
                .iter()
                .chain(&record.combines)
                .copied()
                .collect::<Vec<_>>();
            for parent in parents {
                self.step()?;
                if !reached.insert(parent) {
                    continue;
                }
                let generation = self.record(parent).await?.generation;
                if generation >= lowest {
                    queue.insert((generation, parent));
                }
            }
        }
        Ok(inputs
            .iter()
            .filter(|input| !reached.contains(*input))
            .copied()
            .collect())
    }
}

impl<O: ImmutableObjectStore> RepositoryReader<'_, O> {
    /// Checks a combined revision is what integration authors: its parents are
    /// incomparable, it follows the first of them in [`merge_order`], and its edits are
    /// their merge. Without the first, a revision could combine a revision with its own
    /// descendant, and that descendant's removals would look new to a later merge. A
    /// revision combining nothing passes. The merge is bounded by its own
    /// [`MergeBudget::STANDARD`], not by the caller's.
    pub async fn check_combination(&self, revision: &RevisionRecord) -> Result<MergeWork> {
        self.check_combination_within(revision, MergeBudget::STANDARD)
            .await
    }

    /// [`Self::check_combination`] within `budget`; returns the work it took.
    pub async fn check_combination_within(
        &self,
        revision: &RevisionRecord,
        budget: MergeBudget,
    ) -> Result<MergeWork> {
        if revision.combines.is_empty() {
            return Ok(MergeWork::default());
        }
        let invalid = RepositoryPublicationError::InvalidReferences;
        let predecessor = revision
            .predecessor
            .ok_or(invalid("a combined revision has no predecessor"))?;
        if revision.combines.contains(&predecessor) {
            return Err(invalid("a combined revision combines its own predecessor"));
        }
        let mut merge = RevisionMerge::new(self, revision.change_id, budget);
        let parents = std::iter::once(predecessor)
            .chain(revision.combines.iter().copied())
            .collect::<Vec<_>>();
        if merge.maximal(&parents).await?.len() != parents.len() {
            return Err(invalid(
                "a combined revision's parents descend from one another",
            ));
        }
        let mut ordered = Vec::with_capacity(parents.len());
        for parent in parents {
            ordered.push(merge_order(parent, merge.record(parent).await?));
        }
        ordered.sort();
        if ordered[0].1 != predecessor {
            return Err(invalid(
                "a combined revision does not follow the earliest revision it combines",
            ));
        }
        let ordered = ordered
            .into_iter()
            .map(|(_, parent)| parent)
            .collect::<Vec<_>>();
        if merge.merged_edits(&ordered).await? != revision.edits {
            return Err(invalid(
                "a combined revision's edits are not the merge of the revisions it combines",
            ));
        }
        Ok(merge.work())
    }
}
