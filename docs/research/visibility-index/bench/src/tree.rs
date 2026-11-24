//! A quadtree and a bounding volume hierarchy, both rebuilt from scratch every tick (every entity moves every
//! tick, so updating in place would touch every leaf anyway). They share the node layout and the queries and
//! differ in how a node is split:
//!
//! - **Quadtree**: a node is a square cell, split at its centre into four; a leaf holds up to 16 entities. A
//!   cell keeps splitting while it holds more, up to depth 20, so entities on the same spot end in one leaf.
//! - **BVH**: a node is the tight bounding box of its entities, split in two at the median of the longer axis;
//!   a leaf holds up to 8. The median split always halves the count, so the depth is log2(n) even when many
//!   entities share one position.

use crate::index::{Nearest, SpatialIndex, d2};

#[derive(Clone, Copy, Debug)]
pub enum Split {
    Quad { leaf: usize, max_depth: u32 },
    Median { leaf: usize },
}

#[derive(Clone, Copy, Default)]
struct Node {
    min: [f32; 2],
    max: [f32; 2],
    /// Leaf: entities `first..first + count`. Inner node: children `first..first + count` in `nodes`.
    first: u32,
    count: u32,
    leaf: bool,
}

impl Node {
    /// Squared distance from `p` to the box (0 inside).
    fn d2(&self, p: [f32; 2]) -> f32 {
        let dx = (self.min[0] - p[0]).max(0.0).max(p[0] - self.max[0]);
        let dy = (self.min[1] - p[1]).max(0.0).max(p[1] - self.max[1]);
        dx * dx + dy * dy
    }
}

pub struct Tree {
    split: Split,
    nodes: Vec<Node>,
    order: Vec<u32>,
    xs: Vec<f32>,
    ys: Vec<f32>,
    /// Work list of (node, entities first, count, depth) while building.
    pending: Vec<(usize, usize, usize, u32)>,
}

impl Tree {
    pub fn new(split: Split) -> Self {
        Self {
            split,
            nodes: Vec::new(),
            order: Vec::new(),
            xs: Vec::new(),
            ys: Vec::new(),
            pending: Vec::new(),
        }
    }

    fn bounds(points: &[[f32; 2]], ids: &[u32]) -> ([f32; 2], [f32; 2]) {
        let mut min = [f32::INFINITY; 2];
        let mut max = [f32::NEG_INFINITY; 2];
        for &i in ids {
            let p = points[i as usize];
            for a in 0..2 {
                min[a] = min[a].min(p[a]);
                max[a] = max[a].max(p[a]);
            }
        }
        (min, max)
    }

    fn build(&mut self, points: &[[f32; 2]]) {
        let n = points.len();
        self.nodes.clear();
        self.order.clear();
        self.order.extend(0..n as u32);
        let (mut min, mut max) = Self::bounds(points, &self.order);
        if let Split::Quad { .. } = self.split {
            // A square root cell, so children stay square.
            let side = (max[0] - min[0]).max(max[1] - min[1]);
            max = [min[0] + side, min[1] + side];
        }
        if n == 0 {
            min = [0.0; 2];
            max = [0.0; 2];
        }
        self.nodes.push(Node {
            min,
            max,
            ..Node::default()
        });
        self.pending.clear();
        self.pending.push((0, 0, n, 0));
        while let Some((node, first, count, depth)) = self.pending.pop() {
            let ids = &mut self.order[first..first + count];
            let Node { min, max, .. } = self.nodes[node];
            match self.split {
                Split::Quad { leaf, max_depth } if count > leaf && depth < max_depth => {
                    let mid = [(min[0] + max[0]) * 0.5, (min[1] + max[1]) * 0.5];
                    // Partition into the four quadrants: first by y, then each half by x.
                    let south = partition(ids, |i| points[i as usize][1] < mid[1]);
                    let (lo, hi) = ids.split_at_mut(south);
                    let sw = partition(lo, |i| points[i as usize][0] < mid[0]);
                    let nw = partition(hi, |i| points[i as usize][0] < mid[0]);
                    let parts = [
                        (0, sw, [min[0], min[1]], [mid[0], mid[1]]),
                        (sw, south - sw, [mid[0], min[1]], [max[0], mid[1]]),
                        (south, nw, [min[0], mid[1]], [mid[0], max[1]]),
                        (south + nw, count - south - nw, [mid[0], mid[1]], [max[0], max[1]]),
                    ];
                    let children = self.nodes.len();
                    let mut made = 0;
                    for (offset, len, cmin, cmax) in parts {
                        if len > 0 {
                            self.nodes.push(Node {
                                min: cmin,
                                max: cmax,
                                ..Node::default()
                            });
                            self.pending
                                .push((children + made, first + offset, len, depth + 1));
                            made += 1;
                        }
                    }
                    self.nodes[node].first = children as u32;
                    self.nodes[node].count = made as u32;
                }
                Split::Median { leaf } if count > leaf => {
                    let axis = usize::from(max[1] - min[1] > max[0] - min[0]);
                    let half = count / 2;
                    ids.select_nth_unstable_by(half, |&a, &b| {
                        points[a as usize][axis].total_cmp(&points[b as usize][axis])
                    });
                    let children = self.nodes.len();
                    for (offset, len) in [(0, half), (half, count - half)] {
                        let (cmin, cmax) =
                            Self::bounds(points, &self.order[first + offset..first + offset + len]);
                        self.nodes.push(Node {
                            min: cmin,
                            max: cmax,
                            ..Node::default()
                        });
                        self.pending
                            .push((self.nodes.len() - 1, first + offset, len, depth + 1));
                    }
                    self.nodes[node].first = children as u32;
                    self.nodes[node].count = 2;
                }
                _ => {
                    let n = &mut self.nodes[node];
                    n.leaf = true;
                    n.first = first as u32;
                    n.count = count as u32;
                }
            }
        }
        self.xs.clear();
        self.ys.clear();
        self.xs.extend(self.order.iter().map(|&i| points[i as usize][0]));
        self.ys.extend(self.order.iter().map(|&i| points[i as usize][1]));
    }

    fn leaf(&self, node: &Node, center: [f32; 2], r2: f32, mut visit: impl FnMut(f32, u32)) {
        let range = node.first as usize..(node.first + node.count) as usize;
        for j in range {
            let d = d2(center, [self.xs[j], self.ys[j]]);
            if d <= r2 {
                visit(d, self.order[j]);
            }
        }
    }
}

/// Moves the ids for which `left` holds to the front; returns how many there are.
fn partition(ids: &mut [u32], left: impl Fn(u32) -> bool) -> usize {
    let mut i = 0;
    for j in 0..ids.len() {
        if left(ids[j]) {
            ids.swap(i, j);
            i += 1;
        }
    }
    i
}

impl SpatialIndex for Tree {
    fn update(&mut self, points: &[[f32; 2]]) -> Result<(), String> {
        self.build(points);
        Ok(())
    }

    fn within(&self, center: [f32; 2], radius: f32, out: &mut Vec<(f32, u32)>) {
        let r2 = radius * radius;
        STACK.with_borrow_mut(|stack| {
            stack.clear();
            if !self.nodes.is_empty() {
                stack.push(0);
            }
            while let Some(i) = stack.pop() {
                let node = &self.nodes[i as usize];
                if node.d2(center) > r2 {
                    continue;
                }
                if node.leaf {
                    self.leaf(node, center, r2, |d, id| out.push((d, id)));
                } else {
                    stack.extend(node.first..node.first + node.count);
                }
            }
        });
    }

    /// Best-first: always opens the node nearest to the centre, and stops once the nearest unopened node is
    /// farther than the k-th entity found.
    fn nearest(&self, center: [f32; 2], radius: f32, k: usize, out: &mut Vec<(f32, u32)>) {
        let r2 = radius * radius;
        let mut best = Nearest::new(k);
        let mut open = std::collections::BinaryHeap::new();
        if !self.nodes.is_empty() {
            open.push(std::cmp::Reverse((0u32, 0u32)));
        }
        while let Some(std::cmp::Reverse((bits, i))) = open.pop() {
            let near = f32::from_bits(bits);
            if near > r2 || near > best.bound() {
                break;
            }
            let node = &self.nodes[i as usize];
            if node.leaf {
                self.leaf(node, center, r2, |d, id| best.offer(d, id));
            } else {
                for c in node.first..node.first + node.count {
                    let d = self.nodes[c as usize].d2(center);
                    if d <= r2 {
                        open.push(std::cmp::Reverse((d.to_bits(), c)));
                    }
                }
            }
        }
        best.drain_into(out);
    }
}

thread_local! {
    /// Work stack for `within`, one per thread so queries can run in parallel without allocating.
    static STACK: std::cell::RefCell<Vec<u32>> = const { std::cell::RefCell::new(Vec::new()) };
}
