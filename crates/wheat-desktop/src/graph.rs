//! Lanes for the Changes screen's dependency graph. Changes are listed newest
//! first and a change's bases sit below it, so the graph is laid out top down
//! the way `git log --graph` does it: each lane waits for one change, and a
//! change takes the lane waiting for it, or a free one.

use crate::model::Model;

/// Where one change sits in the graph and which lines cross its row.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GraphRow {
    pub lane: u16,
    /// Lanes that reach the dot from above: its dependents.
    pub from_above: Vec<u16>,
    /// Lanes leaving the dot downwards, towards its bases.
    pub to_below: Vec<u16>,
    /// Lanes passing the row without touching the dot.
    pub through: Vec<u16>,
}

impl GraphRow {
    /// Every lane with a line below this row: what a revision row drawn under
    /// the change continues.
    pub fn below(&self) -> impl Iterator<Item = u16> + '_ {
        self.to_below.iter().chain(&self.through).copied()
    }
}

#[derive(Clone, Debug, Default)]
pub struct Graph {
    pub rows: Vec<GraphRow>,
}

impl Graph {
    /// Lays out `model.changes` in one pass. The work per change is the number
    /// of lanes open at its row, so a long published history, where every
    /// change is built on the next, stays one lane wide and linear.
    pub fn layout(model: &Model) -> Self {
        // `waiting[lane]` is the change that lane leads down to.
        let mut waiting: Vec<Option<usize>> = Vec::new();
        let mut rows = Vec::with_capacity(model.changes.len());
        for (index, change) in model.changes.iter().enumerate() {
            let mut row = GraphRow::default();
            for (lane, wait) in waiting.iter_mut().enumerate() {
                if *wait == Some(index) {
                    row.from_above.push(lane as u16);
                    *wait = None;
                }
            }
            let lane = match row.from_above.first() {
                Some(&lane) => lane as usize,
                None => match waiting.iter().position(Option::is_none) {
                    Some(lane) => lane,
                    None => {
                        waiting.push(None);
                        waiting.len() - 1
                    }
                },
            };
            row.lane = lane as u16;
            // Bases above this row would be a cycle or a stale index; they get
            // no line rather than one that never ends.
            let mut bases = change
                .depends_on
                .iter()
                .copied()
                .filter(|&base| base > index);
            if let Some(first) = bases.next() {
                match waiting.iter().position(|&wait| wait == Some(first)) {
                    Some(other) => row.to_below.push(other as u16),
                    None => {
                        waiting[lane] = Some(first);
                        row.to_below.push(lane as u16);
                    }
                }
            }
            for base in bases {
                let target = match waiting.iter().position(|&wait| wait == Some(base)) {
                    Some(other) => other,
                    None => {
                        let free = match waiting.iter().position(Option::is_none) {
                            Some(free) => free,
                            None => {
                                waiting.push(None);
                                waiting.len() - 1
                            }
                        };
                        waiting[free] = Some(base);
                        free
                    }
                };
                if !row.to_below.contains(&(target as u16)) {
                    row.to_below.push(target as u16);
                }
            }
            for (other, wait) in waiting.iter().enumerate() {
                if wait.is_some() && !row.to_below.contains(&(other as u16)) {
                    row.through.push(other as u16);
                }
            }
            while waiting.last() == Some(&None) {
                waiting.pop();
            }
            rows.push(row);
        }
        Self { rows }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Change;

    fn model(bases: &[&[usize]]) -> Model {
        Model {
            changes: bases
                .iter()
                .map(|bases| Change {
                    depends_on: bases.to_vec(),
                    ..Change::default()
                })
                .collect(),
            ..Model::default()
        }
    }

    /// How many lanes the graph uses.
    fn width(graph: &Graph) -> u16 {
        graph
            .rows
            .iter()
            .flat_map(|row| std::iter::once(row.lane).chain(row.below()))
            .max()
            .map_or(0, |lane| lane + 1)
    }

    #[test]
    fn a_linear_history_stays_in_one_lane() {
        let graph = Graph::layout(&model(&[&[1], &[2], &[3], &[]]));
        assert_eq!(width(&graph), 1);
        assert!(
            graph
                .rows
                .iter()
                .all(|row| row.lane == 0 && row.through.is_empty())
        );
        assert_eq!(graph.rows[0].from_above, Vec::<u16>::new());
        assert_eq!(graph.rows[1].from_above, vec![0]);
        assert_eq!(graph.rows[3].to_below, Vec::<u16>::new());
    }

    #[test]
    fn two_dependents_of_one_base_join_at_it() {
        // 0 and 1 are both built on 2.
        let graph = Graph::layout(&model(&[&[2], &[2], &[]]));
        assert_eq!(graph.rows[0].lane, 0);
        assert_eq!(graph.rows[1].lane, 1);
        // The second dependent's line runs into the lane already waiting for 2.
        assert_eq!(graph.rows[1].to_below, vec![0]);
        assert_eq!(graph.rows[2].from_above, vec![0]);
        assert_eq!(width(&graph), 2);
    }

    #[test]
    fn a_side_branch_passes_the_rows_between() {
        // 0 is built on 3; 1 and 2 form their own stack.
        let graph = Graph::layout(&model(&[&[3], &[2], &[], &[]]));
        assert_eq!(graph.rows[1].lane, 1);
        assert_eq!(graph.rows[1].through, vec![0]);
        assert_eq!(graph.rows[2].lane, 1);
        assert_eq!(graph.rows[2].through, vec![0]);
        assert_eq!(graph.rows[3].lane, 0);
        assert_eq!(graph.rows[3].from_above, vec![0]);
    }

    #[test]
    fn a_change_on_two_bases_opens_a_second_lane() {
        let graph = Graph::layout(&model(&[&[1, 2], &[], &[]]));
        assert_eq!(graph.rows[0].to_below, vec![0, 1]);
        assert_eq!(graph.rows[1].through, vec![1]);
        assert_eq!(graph.rows[2].lane, 1);
        assert_eq!(graph.rows[2].from_above, vec![1]);
    }

    #[test]
    fn a_base_listed_above_its_dependent_gets_no_line() {
        let graph = Graph::layout(&model(&[&[], &[0]]));
        assert!(graph.rows[1].to_below.is_empty());
        assert_eq!(width(&graph), 1);
    }

    #[test]
    fn freed_lanes_are_reused() {
        // 0 → 1 in lane 0; 2 independent; 3 independent.
        let graph = Graph::layout(&model(&[&[1], &[], &[], &[]]));
        assert!(graph.rows.iter().all(|row| row.lane == 0));
    }
}
