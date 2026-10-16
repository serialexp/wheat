//! The application order index against a brute force over a chain of applications:
//! every entry, and the highest ChangeId over every run of last positions.

use std::collections::BTreeMap;

use wheat_core::{ApplicationId, ApplicationOrder, ChangeId, OrderIndexMismatch, RangeMax};

/// A composition chain: the application at each position, its change and its order.
struct Chain {
    ids: Vec<ApplicationId>,
    changes: Vec<ChangeId>,
    orders: BTreeMap<ApplicationId, ApplicationOrder>,
}

impl Chain {
    /// `length` applications whose ChangeIds rise and fall, so no window's highest is
    /// simply its first or last position.
    fn new(length: u64) -> Self {
        let mut chain = Self {
            ids: Vec::new(),
            changes: Vec::new(),
            orders: BTreeMap::new(),
        };
        for position in 0..length {
            let change = ChangeId::from_bytes(
                ((position * 7_919) % 1_009)
                    .to_be_bytes()
                    .repeat(2)
                    .try_into()
                    .unwrap(),
            );
            let mut order = ApplicationOrder::begin(change, position, chain.ids.last().copied());
            let mut reads = 0;
            while let Some(id) = order.wanted() {
                order.extend(&chain.orders[&id]).unwrap();
                reads += 1;
            }
            // One read per level after the first: logarithmic in the position.
            assert_eq!(reads, ApplicationOrder::window_levels(position) - 1);
            assert!(order.is_well_formed());
            let mut digest = [0; 32];
            digest[..8].copy_from_slice(&position.to_be_bytes());
            let id = ApplicationId::from_digest(digest);
            chain.ids.push(id);
            chain.changes.push(change);
            chain.orders.insert(id, order);
        }
        chain
    }

    fn order(&self, position: u64) -> &ApplicationOrder {
        &self.orders[&self.ids[position as usize]]
    }

    /// The highest ChangeId at positions `from` up to `size`, from the index, and the
    /// number of applications read.
    fn highest(&self, from: u64, size: u64) -> (Option<ChangeId>, usize) {
        let mut query = RangeMax::new(from, size);
        if query.is_empty() {
            return (None, 0);
        }
        let mut next = Some(self.ids[size as usize - 1]);
        let mut reads = 0;
        while let Some(id) = next {
            next = query.fold(&self.orders[&id]).unwrap();
            reads += 1;
        }
        (query.highest(), reads)
    }
}

#[test]
fn every_entry_matches_a_brute_force() {
    let chain = Chain::new(1_001);
    for position in [0_u64, 1, 2, 3, 4, 7, 8, 1_000] {
        let order = chain.order(position);
        assert_eq!(order.position, position);
        assert_eq!(
            order.previous,
            position.checked_sub(1).map(|p| chain.ids[p as usize])
        );
        for (level, skip) in order.skip.iter().enumerate() {
            assert_eq!(*skip, chain.ids[(position - (1 << level)) as usize]);
        }
        assert_eq!(order.skip.len(), ApplicationOrder::skip_levels(position));
        for (level, highest) in order.window_max.iter().enumerate() {
            let start = (position + 1).saturating_sub(1 << level) as usize;
            let expected = chain.changes[start..=position as usize].iter().max();
            assert_eq!(Some(highest), expected, "position {position} level {level}");
        }
        assert_eq!(
            order.window_max.len(),
            ApplicationOrder::window_levels(position)
        );
    }
}

#[test]
fn the_highest_change_over_any_run_of_last_positions_matches_a_brute_force() {
    let chain = Chain::new(1_001);
    for size in [0_u64, 1, 2, 3, 4, 5, 8, 9, 100, 1_000, 1_001] {
        for from in 0..=size + 1 {
            let (highest, reads) = chain.highest(from, size);
            let expected = chain
                .changes
                .get(from.min(size) as usize..size as usize)
                .and_then(|run| run.iter().max().copied());
            assert_eq!(highest, expected, "size {size} from {from}");
            // One read per set bit of the run's length.
            let length = size.saturating_sub(from);
            assert_eq!(reads, length.count_ones() as usize);
        }
    }
}

#[test]
fn a_node_at_the_wrong_position_is_refused() {
    let chain = Chain::new(9);
    let change = ChangeId::from_bytes([9; 16]);
    let mut order = ApplicationOrder::begin(change, 9, Some(chain.ids[8]));
    // The first level wants position 8; position 7 is refused.
    assert_eq!(order.wanted(), Some(chain.ids[8]));
    assert_eq!(order.extend(chain.order(7)), Err(OrderIndexMismatch));

    let mut query = RangeMax::new(0, 9);
    assert_eq!(query.fold(chain.order(7)), Err(OrderIndexMismatch));
    // A malformed node is refused rather than indexed past its end.
    let mut short = chain.order(8).clone();
    short.window_max.pop();
    assert_eq!(query.fold(&short), Err(OrderIndexMismatch));
}
