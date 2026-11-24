//! Ready-made structures: the R*-tree of `rstar` and the k-d trees of `kiddo`, each either rebuilt every tick
//! or updated in place (every entity removed and inserted at its new position).

use std::num::NonZero;

use kiddo::{ImmutableKdTree, MutableKdTree, SquaredEuclidean};
use rstar::RTree;
use rstar::primitives::GeomWithData;

use crate::index::{SpatialIndex, keep_nearest};

type Entry = GeomWithData<[f32; 2], u32>;

/// `rstar` R*-tree. `rebuild`: `bulk_load` every tick (packed, the fastest way to build one); otherwise
/// `remove` and `insert` each entity that moved.
pub struct RStar {
    tree: RTree<Entry>,
    previous: Vec<[f32; 2]>,
    rebuild: bool,
}

impl RStar {
    pub fn new(rebuild: bool) -> Self {
        Self {
            tree: RTree::new(),
            previous: Vec::new(),
            rebuild,
        }
    }
}

impl SpatialIndex for RStar {
    fn update(&mut self, points: &[[f32; 2]]) -> Result<(), String> {
        if self.rebuild || self.previous.len() != points.len() {
            let entries = points
                .iter()
                .enumerate()
                .map(|(i, &p)| Entry::new(p, i as u32))
                .collect();
            self.tree = RTree::bulk_load(entries);
        } else {
            for (i, (&p, old)) in points.iter().zip(&mut self.previous).enumerate() {
                if p != *old {
                    if self.tree.remove(&Entry::new(*old, i as u32)).is_none() {
                        return Err(format!("rstar lost entity {i}"));
                    }
                    self.tree.insert(Entry::new(p, i as u32));
                }
            }
        }
        self.previous.clear();
        self.previous.extend_from_slice(points);
        Ok(())
    }

    fn within(&self, center: [f32; 2], radius: f32, out: &mut Vec<(f32, u32)>) {
        for e in self.tree.locate_within_distance(center, radius * radius) {
            out.push((crate::index::d2(center, *e.geom()), e.data));
        }
    }

    fn nearest(&self, center: [f32; 2], radius: f32, k: usize, out: &mut Vec<(f32, u32)>) {
        // The iterator yields in order of distance; take the k-th and any tied with it, then cut by id.
        let r2 = radius * radius;
        let start = out.len();
        let mut kth = f32::INFINITY;
        for (e, d) in self.tree.nearest_neighbor_iter_with_distance_2(center) {
            if d > r2 || d > kth {
                break;
            }
            out.push((d, e.data));
            if out.len() - start == k {
                kth = d;
            }
        }
        let mut tail = out.split_off(start);
        keep_nearest(&mut tail, k);
        out.extend(tail);
    }
}

/// `kiddo` k-d tree. `Immutable`: rebuilt from the positions every tick. `Mutable`: `remove` and `add` each
/// entity that moved; its buckets hold at most 32 entities that share a coordinate, so it refuses more.
pub enum Kiddo {
    Immutable(Option<ImmutableKdTree<f32, 2>>),
    Mutable(MutableKdTree<f32, 2>, Vec<[f32; 2]>),
}

impl Kiddo {
    pub fn immutable() -> Self {
        Self::Immutable(None)
    }

    pub fn mutable() -> Self {
        Self::Mutable(MutableKdTree::default(), Vec::new())
    }
}

impl SpatialIndex for Kiddo {
    fn update(&mut self, points: &[[f32; 2]]) -> Result<(), String> {
        match self {
            Self::Immutable(tree) => {
                *tree = Some(ImmutableKdTree::new_from_slice(points).map_err(|e| format!("kiddo: {e:?}"))?);
            }
            Self::Mutable(tree, previous) => {
                if previous.len() != points.len() {
                    *tree = MutableKdTree::default();
                    for (i, p) in points.iter().enumerate() {
                        tree.add(p, i as u32).map_err(|e| format!("kiddo add: {e:?}"))?;
                    }
                } else {
                    for (i, (p, old)) in points.iter().zip(previous.iter()).enumerate() {
                        if p != old {
                            tree.remove(old, i as u32);
                            tree.add(p, i as u32).map_err(|e| format!("kiddo add: {e:?}"))?;
                        }
                    }
                }
                previous.clear();
                previous.extend_from_slice(points);
            }
        }
        Ok(())
    }

    fn within(&self, center: [f32; 2], radius: f32, out: &mut Vec<(f32, u32)>) {
        let r2 = radius * radius;
        match self {
            Self::Immutable(Some(tree)) => tree
                .query(&center)
                .within::<SquaredEuclidean<f32>>(r2)
                .unsorted()
                .visit(|r| out.push((r.distance, r.item))),
            Self::Mutable(tree, _) => tree
                .query(&center)
                .within::<SquaredEuclidean<f32>>(r2)
                .unsorted()
                .visit(|r| out.push((r.distance, r.item))),
            Self::Immutable(None) => {}
        }
    }

    fn nearest(&self, center: [f32; 2], radius: f32, k: usize, out: &mut Vec<(f32, u32)>) {
        let r2 = radius * radius;
        let Some(k) = NonZero::new(k) else { return };
        let found = match self {
            Self::Immutable(Some(tree)) => tree
                .query(&center)
                .nearest_n::<SquaredEuclidean<f32>>(k)
                .within(r2)
                .unsorted()
                .execute(),
            Self::Mutable(tree, _) => tree
                .query(&center)
                .nearest_n::<SquaredEuclidean<f32>>(k)
                .within(r2)
                .unsorted()
                .execute(),
            Self::Immutable(None) => return,
        };
        out.extend(found.into_iter().map(|r| (r.distance, r.item)));
    }
}
