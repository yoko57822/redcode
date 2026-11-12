//! Lane assignment for the commit graph. Pure; fed commits in `git log --date-order` order.

use super::model::Sha;

pub const LANE_COLORS: u8 = 8;

/// A line segment between lane `from` and lane `to`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Edge {
    pub from: u16,
    pub to: u16,
    pub color: u8,
}

/// `top` edges run from the row's upper boundary (x = `from` lane) to the row's
/// centre (x = `to` lane); `bottom` edges run from the centre (x = `from`) to the
/// lower boundary (x = `to`).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct GraphRow {
    pub column: u16,
    pub color: u8,
    pub top: Vec<Edge>,
    pub bottom: Vec<Edge>,
}

#[derive(Clone, Debug)]
struct Lane {
    /// The commit this lane is waiting for.
    expect: Sha,
    color: u8,
}

pub struct GraphLayout {
    lanes: Vec<Option<Lane>>,
    next_color: u8,
    max_width: u16,
}

impl Default for GraphLayout {
    fn default() -> Self {
        Self::new()
    }
}

impl GraphLayout {
    pub fn new() -> Self {
        Self {
            lanes: Vec::new(),
            next_color: 0,
            max_width: 0,
        }
    }

    pub fn max_width(&self) -> u16 {
        self.max_width
    }

    pub fn push(&mut self, id: &Sha, parents: &[Sha]) -> GraphRow {
        // 1. Lanes waiting for this commit.
        let hits: Vec<usize> = self.waiting_lanes(id).collect();

        // 2. Column and colour.
        let column = match hits.first() {
            Some(&hit) => hit,
            None => self.first_free_slot(None),
        };
        let color = match hits.first() {
            Some(&hit) => self.lane_color(hit),
            None => self.take_color(),
        };

        // 3. Edges entering the row.
        let top: Vec<Edge> = self
            .lanes
            .iter()
            .enumerate()
            .filter_map(|(i, lane)| {
                let lane = lane.as_ref()?;
                let to = if hits.contains(&i) { column } else { i };
                Some(edge(i, to, lane.color))
            })
            .collect();

        // 4. Free the hit lanes; the remaining occupied lanes pass through.
        for &hit in &hits {
            self.lanes[hit] = None;
        }
        let mut pass: Vec<usize> = (0..self.lanes.len())
            .filter(|&i| self.lanes[i].is_some())
            .collect();

        let mut bottom = Vec::new();

        // 5. First parent continues the commit's line.
        if let Some(p0) = parents.first() {
            let waiting = self.waiting_lanes(p0).next();
            match waiting {
                Some(j) if j < column => {
                    bottom.push(edge(column, j, self.lane_color(j)));
                }
                Some(j) => {
                    let old_color = self.lane_color(j);
                    self.lanes[column] = Some(Lane {
                        expect: p0.clone(),
                        color,
                    });
                    self.lanes[j] = None;
                    bottom.push(edge(column, column, color));
                    bottom.push(edge(j, column, old_color));
                    pass.retain(|&i| i != j);
                }
                None => {
                    self.lanes[column] = Some(Lane {
                        expect: p0.clone(),
                        color,
                    });
                    bottom.push(edge(column, column, color));
                }
            }
        }

        // 6. Remaining parents join existing lanes or open new ones.
        for parent in parents.iter().skip(1) {
            let waiting = self.waiting_lanes(parent).next();
            match waiting {
                Some(j) => bottom.push(edge(column, j, self.lane_color(j))),
                None => {
                    let k = self.first_free_slot(Some(column));
                    let new_color = self.take_color();
                    self.lanes[k] = Some(Lane {
                        expect: parent.clone(),
                        color: new_color,
                    });
                    bottom.push(edge(column, k, new_color));
                }
            }
        }

        // 7. Pass-through lanes, then trim trailing free slots. The width is taken
        // before trimming so that the column and every edge of this row fit.
        for &i in &pass {
            bottom.push(edge(i, i, self.lane_color(i)));
        }
        self.max_width = self.max_width.max(self.lanes.len() as u16);
        while matches!(self.lanes.last(), Some(None)) {
            self.lanes.pop();
        }

        GraphRow {
            column: column as u16,
            color,
            top,
            bottom,
        }
    }

    fn waiting_lanes<'a>(&'a self, id: &'a Sha) -> impl Iterator<Item = usize> + 'a {
        self.lanes
            .iter()
            .enumerate()
            .filter_map(move |(i, lane)| match lane {
                Some(lane) if lane.expect == *id => Some(i),
                _ => None,
            })
    }

    /// First free slot other than `skip`; appends a new slot when none is free.
    fn first_free_slot(&mut self, skip: Option<usize>) -> usize {
        let free = (0..self.lanes.len()).find(|&i| Some(i) != skip && self.lanes[i].is_none());
        free.unwrap_or_else(|| {
            self.lanes.push(None);
            self.lanes.len() - 1
        })
    }

    fn lane_color(&self, i: usize) -> u8 {
        self.lanes[i].as_ref().map_or(0, |lane| lane.color)
    }

    fn take_color(&mut self) -> u8 {
        let color = self.next_color;
        self.next_color = (self.next_color + 1) % LANE_COLORS;
        color
    }
}

fn edge(from: usize, to: usize, color: u8) -> Edge {
    Edge {
        from: from as u16,
        to: to as u16,
        color,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sha(s: &str) -> Sha {
        Sha::from(s)
    }

    fn pairs(edges: &[Edge]) -> Vec<(u16, u16)> {
        let mut pairs: Vec<(u16, u16)> = edges.iter().map(|e| (e.from, e.to)).collect();
        pairs.sort_unstable();
        pairs
    }

    fn sorted(mut pairs: Vec<(u16, u16)>) -> Vec<(u16, u16)> {
        pairs.sort_unstable();
        pairs
    }

    #[test]
    fn linear_history_stays_in_column_zero() {
        let mut layout = GraphLayout::new();
        let rows = [
            layout.push(&sha("c3"), &[sha("c2")]),
            layout.push(&sha("c2"), &[sha("c1")]),
            layout.push(&sha("c1"), &[]),
        ];
        for row in &rows {
            assert_eq!(row.column, 0);
            for e in row.top.iter().chain(&row.bottom) {
                assert_eq!((e.from, e.to), (0, 0));
            }
        }
        assert_eq!(pairs(&rows[0].top), vec![]);
        assert_eq!(pairs(&rows[0].bottom), vec![(0, 0)]);
        assert_eq!(pairs(&rows[1].top), vec![(0, 0)]);
        assert_eq!(pairs(&rows[1].bottom), vec![(0, 0)]);
        assert_eq!(pairs(&rows[2].top), vec![(0, 0)]);
        assert_eq!(pairs(&rows[2].bottom), vec![]);
        assert_eq!(layout.max_width(), 1);
    }

    #[test]
    fn merge_with_two_branches() {
        let mut layout = GraphLayout::new();
        let m = layout.push(&sha("M"), &[sha("A"), sha("B")]);
        let b = layout.push(&sha("B"), &[sha("C")]);
        let a = layout.push(&sha("A"), &[sha("C")]);
        let c = layout.push(&sha("C"), &[]);

        assert_eq!(m.column, 0);
        assert_eq!(pairs(&m.top), vec![]);
        assert_eq!(pairs(&m.bottom), sorted(vec![(0, 0), (0, 1)]));

        assert_eq!(b.column, 1);
        assert_eq!(pairs(&b.top), sorted(vec![(0, 0), (1, 1)]));
        assert_eq!(pairs(&b.bottom), sorted(vec![(1, 1), (0, 0)]));

        assert_eq!(a.column, 0);
        assert_eq!(pairs(&a.bottom), sorted(vec![(0, 0), (1, 0)]));

        assert_eq!(c.column, 0);
        assert_eq!(pairs(&c.top), vec![(0, 0)]);
        assert_eq!(pairs(&c.bottom), vec![]);

        assert_eq!(layout.max_width(), 2);

        // Each line keeps its colour: the merged-in branch differs from the trunk.
        assert_eq!(m.color, a.color);
        assert_ne!(m.color, b.color);
        assert!(m.bottom.contains(&Edge {
            from: 0,
            to: 1,
            color: b.color
        }));
        assert!(a.bottom.contains(&Edge {
            from: 1,
            to: 0,
            color: b.color
        }));
    }

    #[test]
    fn unrelated_roots_reuse_column_zero() {
        let mut layout = GraphLayout::new();
        let first = layout.push(&sha("r1"), &[]);
        let second = layout.push(&sha("r2"), &[]);
        assert_eq!(first.column, 0);
        assert_eq!(second.column, 0);
        assert!(first.top.is_empty() && first.bottom.is_empty());
        assert!(second.top.is_empty() && second.bottom.is_empty());
        assert_eq!(layout.max_width(), 1);
    }
}
