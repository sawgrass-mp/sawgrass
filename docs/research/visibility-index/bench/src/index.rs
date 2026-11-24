//! The spatial indexes under test, behind one trait, and the ones that need no tree: brute force, the server's
//! current grid, and a dense grid rebuilt every tick.

use sawgrass_server::interest::{EntityId, Point, SpatialGrid};

/// A 2D point index over entities `0..n`, refreshed once per tick with every position.
pub trait SpatialIndex: Sync {
    /// Brings the index up to date with `points` (entity `i` is at `points[i]`). The same ids are passed
    /// every tick; every entity may have moved.
    fn update(&mut self, points: &[[f32; 2]]) -> Result<(), String>;

    /// Pushes `(squared distance, id)` for every entity within `radius` of `center` (inclusive), in any order.
    fn within(&self, center: [f32; 2], radius: f32, out: &mut Vec<(f32, u32)>);

    /// Pushes the `k` entities nearest to `center` within `radius`, as `(squared distance, id)`, in any order.
    /// When several are at the k-th distance, which of them are returned is up to the index.
    fn nearest(&self, center: [f32; 2], radius: f32, k: usize, out: &mut Vec<(f32, u32)>);
}

pub fn d2(a: [f32; 2], b: [f32; 2]) -> f32 {
    let dx = a[0] - b[0];
    let dy = a[1] - b[1];
    dx * dx + dy * dy
}

/// Orders `(squared distance, id)` pairs by distance, then id; distances are never negative or NaN here, so
/// the bits of the float order like the float.
pub fn key(d2: f32, id: u32) -> u64 {
    (u64::from(d2.to_bits()) << 32) | u64::from(id)
}

/// Cuts `out` to its `k` nearest by (distance, id). The fallback for indexes with no k-nearest search.
pub fn keep_nearest(out: &mut Vec<(f32, u32)>, k: usize) {
    if out.len() > k {
        out.select_nth_unstable_by_key(k - 1, |&(d, id)| key(d, id));
        out.truncate(k);
    }
}

/// Keeps the `k` smallest keys seen; a max-heap so the k-th is on top.
pub struct Nearest {
    heap: std::collections::BinaryHeap<u64>,
    k: usize,
}

impl Nearest {
    pub fn new(k: usize) -> Self {
        Self {
            heap: std::collections::BinaryHeap::with_capacity(k + 1),
            k,
        }
    }

    pub fn offer(&mut self, d2: f32, id: u32) {
        let key = key(d2, id);
        if self.heap.len() < self.k {
            self.heap.push(key);
        } else if let Some(mut top) = self.heap.peek_mut()
            && key < *top
        {
            *top = key;
        }
    }

    /// The k-th squared distance once k entities are held; infinity before.
    pub fn bound(&self) -> f32 {
        if self.heap.len() < self.k {
            f32::INFINITY
        } else {
            self.heap
                .peek()
                .map_or(f32::INFINITY, |&top| f32::from_bits((top >> 32) as u32))
        }
    }

    pub fn drain_into(self, out: &mut Vec<(f32, u32)>) {
        out.extend(
            self.heap
                .into_iter()
                .map(|key| (f32::from_bits((key >> 32) as u32), key as u32)),
        );
    }
}

/// Checks every entity: the reference for correctness and the floor when everyone is in one place.
#[derive(Default)]
pub struct Brute {
    points: Vec<[f32; 2]>,
}

impl SpatialIndex for Brute {
    fn update(&mut self, points: &[[f32; 2]]) -> Result<(), String> {
        self.points.clear();
        self.points.extend_from_slice(points);
        Ok(())
    }

    fn within(&self, center: [f32; 2], radius: f32, out: &mut Vec<(f32, u32)>) {
        let r2 = radius * radius;
        for (i, &p) in self.points.iter().enumerate() {
            let d = d2(center, p);
            if d <= r2 {
                out.push((d, i as u32));
            }
        }
    }

    fn nearest(&self, center: [f32; 2], radius: f32, k: usize, out: &mut Vec<(f32, u32)>) {
        let start = out.len();
        self.within(center, radius, out);
        let mut tail = out.split_off(start);
        keep_nearest(&mut tail, k);
        out.extend(tail);
    }
}

/// The server's `SpatialGrid` as it is today (hashed 100 m cells, entities moved in place with `upsert`).
pub struct CurrentGrid {
    grid: SpatialGrid,
}

impl CurrentGrid {
    pub fn new(cell: f32) -> Self {
        Self {
            grid: SpatialGrid::new(cell),
        }
    }

    pub fn grid(&self) -> &SpatialGrid {
        &self.grid
    }
}

impl SpatialIndex for CurrentGrid {
    fn update(&mut self, points: &[[f32; 2]]) -> Result<(), String> {
        for (i, p) in points.iter().enumerate() {
            self.grid.upsert(EntityId(i as u32), Point::new(p[0], p[1]));
        }
        Ok(())
    }

    fn within(&self, center: [f32; 2], radius: f32, out: &mut Vec<(f32, u32)>) {
        self.grid
            .for_each_within(Point::new(center[0], center[1]), radius, |id, _, d| {
                out.push((d, id.0))
            });
    }

    fn nearest(&self, center: [f32; 2], radius: f32, k: usize, out: &mut Vec<(f32, u32)>) {
        let start = out.len();
        self.within(center, radius, out);
        let mut tail = out.split_off(start);
        keep_nearest(&mut tail, k);
        out.extend(tail);
    }
}

/// A grid over fixed map bounds stored as one array: every tick the entities are counted into cells and
/// written out sorted by cell (row-major), so a query reads one contiguous run of entities per row of cells.
/// Positions outside the bounds are clamped to the edge cells.
pub struct DenseGrid {
    origin: [f32; 2],
    cell: f32,
    inverse: f32,
    nx: usize,
    ny: usize,
    /// `start[c]..start[c + 1]` are the entities of cell `c`.
    start: Vec<u32>,
    xs: Vec<f32>,
    ys: Vec<f32>,
    ids: Vec<u32>,
    cell_of: Vec<u32>,
}

impl DenseGrid {
    pub fn new(origin: [f32; 2], size: f32, cell: f32) -> Self {
        let n = (size / cell).ceil().max(1.0) as usize;
        Self {
            origin,
            cell,
            inverse: 1.0 / cell,
            nx: n,
            ny: n,
            start: vec![0; n * n + 1],
            xs: Vec::new(),
            ys: Vec::new(),
            ids: Vec::new(),
            cell_of: Vec::new(),
        }
    }

    fn coord(&self, v: f32, origin: f32, n: usize) -> usize {
        // Saturating cast: below the origin (or NaN) gives 0, far above gives the last cell.
        (((v - origin) * self.inverse).floor().max(0.0) as usize).min(n - 1)
    }

    fn scan(
        &self,
        range: std::ops::Range<usize>,
        center: [f32; 2],
        r2: f32,
        mut visit: impl FnMut(f32, u32),
    ) {
        for j in range {
            let d = d2(center, [self.xs[j], self.ys[j]]);
            if d <= r2 {
                visit(d, self.ids[j]);
            }
        }
    }
}

impl SpatialIndex for DenseGrid {
    fn update(&mut self, points: &[[f32; 2]]) -> Result<(), String> {
        let n = points.len();
        self.cell_of.clear();
        self.start.iter_mut().for_each(|s| *s = 0);
        for p in points {
            let c = self.coord(p[1], self.origin[1], self.ny) * self.nx
                + self.coord(p[0], self.origin[0], self.nx);
            self.cell_of.push(c as u32);
            self.start[c + 1] += 1;
        }
        for c in 1..self.start.len() {
            self.start[c] += self.start[c - 1];
        }
        self.xs.resize(n, 0.0);
        self.ys.resize(n, 0.0);
        self.ids.resize(n, 0);
        // Scatter, using start[c] as a cursor, then shift the cursors back.
        for (i, p) in points.iter().enumerate() {
            let c = self.cell_of[i] as usize;
            let j = self.start[c] as usize;
            self.xs[j] = p[0];
            self.ys[j] = p[1];
            self.ids[j] = i as u32;
            self.start[c] += 1;
        }
        for c in (1..self.start.len()).rev() {
            self.start[c] = self.start[c - 1];
        }
        self.start[0] = 0;
        Ok(())
    }

    fn within(&self, center: [f32; 2], radius: f32, out: &mut Vec<(f32, u32)>) {
        let r2 = radius * radius;
        let x0 = self.coord(center[0] - radius, self.origin[0], self.nx);
        let x1 = self.coord(center[0] + radius, self.origin[0], self.nx);
        let y0 = self.coord(center[1] - radius, self.origin[1], self.ny);
        let y1 = self.coord(center[1] + radius, self.origin[1], self.ny);
        for y in y0..=y1 {
            let row = y * self.nx;
            let range = self.start[row + x0] as usize..self.start[row + x1 + 1] as usize;
            self.scan(range, center, r2, |d, id| out.push((d, id)));
        }
    }

    /// Searches rings of cells outwards from the centre's cell, collecting every entity within the radius,
    /// and stops once the next ring cannot hold anything nearer than the k-th collected; then cuts to k.
    fn nearest(&self, center: [f32; 2], radius: f32, k: usize, out: &mut Vec<(f32, u32)>) {
        let r2 = radius * radius;
        let cx = self.coord(center[0], self.origin[0], self.nx) as isize;
        let cy = self.coord(center[1], self.origin[1], self.ny) as isize;
        let rings = (radius * self.inverse).ceil() as isize + 1;
        let (nx, ny) = (self.nx as isize, self.ny as isize);
        let start = out.len();
        let mut bound = f32::INFINITY;
        for ring in 0..=rings {
            // Every entity of ring `ring` is at least (ring - 1) cells away from the centre.
            let reach = (ring - 1).max(0) as f32 * self.cell;
            if reach * reach > r2 || reach * reach > bound {
                break;
            }
            for y in (cy - ring).max(0)..=(cy + ring).min(ny - 1) {
                let row = y as usize * self.nx;
                let edge = y == cy - ring || y == cy + ring;
                let mut span = |x0: isize, x1: isize| {
                    let (x0, x1) = (x0.max(0), x1.min(nx - 1));
                    if x0 <= x1 {
                        let range = self.start[row + x0 as usize] as usize
                            ..self.start[row + x1 as usize + 1] as usize;
                        self.scan(range, center, r2, |d, id| out.push((d, id)));
                    }
                };
                if edge {
                    span(cx - ring, cx + ring);
                } else {
                    span(cx - ring, cx - ring);
                    if ring > 0 {
                        span(cx + ring, cx + ring);
                    }
                }
            }
            if out.len() - start >= k {
                let found = &mut out[start..];
                found.select_nth_unstable_by_key(k - 1, |&(d, id)| key(d, id));
                bound = found[k - 1].0;
                out.truncate(start + k);
            }
        }
    }
}
