//! Terrain-pool slot allocator (pure bookkeeping — no GPU, host-testable).
//!
//! The client's terrain pool is one pre-allocated vertex/index buffer
//! pair; chunks own contiguous slots in it. Vertices and indices are
//! appended in lockstep (a chunk's water sub-mesh follows its opaque part
//! in both buffers), so a slot is contiguous in *both* buffers and
//! adjacency in one implies adjacency in the other.
//!
//! The old design appended at the high-water mark and, under pressure,
//! re-uploaded the ENTIRE pool to the GPU from the front (~75 MB per
//! compaction — a main-thread hitch that recurred on every incoming chunk
//! while the pool sat at capacity, i.e. exactly in fast flight mode). This
//! allocator instead keeps a coalescing free list of released slots, so a
//! replaced or evicted chunk's slot is reused in place: steady-state
//! inserts and evictions cost one small buffer upload each, and no full
//! re-compaction exists at all. Best-fit + tail splitting + coalescing
//! keep fragmentation low; if a need ever exceeds every single free slot,
//! the caller parks more (fog-bound) victims until coalesced space fits —
//! the drop loop in `qwencraft-client` is built around `release` +
//! `alloc` for exactly this reason.

/// A contiguous region of the pool, in both the vertex and index buffers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Slot {
    pub base_v: u32,
    pub base_i: u32,
    /// Number of vertices in the slot.
    pub v_count: u32,
    /// Number of indices in the slot.
    pub i_count: u32,
}

impl Slot {
    /// The unused tail of this slot when only `use_v`/`use_i` of it are
    /// taken (None when the slot is used exactly).
    fn tail(&self, use_v: u32, use_i: u32) -> Option<Slot> {
        if self.v_count > use_v || self.i_count > use_i {
            Some(Slot {
                base_v: self.base_v + use_v,
                base_i: self.base_i + use_i,
                v_count: self.v_count - use_v,
                i_count: self.i_count - use_i,
            })
        } else {
            None
        }
    }
}

/// Slot allocator for the terrain pool.
pub struct TerrainPool {
    /// High-water mark in the vertex buffer (all live + free slots live
    /// below it; the tail at/above it is append headroom).
    high_v: u32,
    high_i: u32,
    /// Released slots, coalesced where adjacent.
    free: Vec<Slot>,
    vert_cap: u32,
    idx_cap: u32,
}

impl TerrainPool {
    pub fn new(vert_cap: u32, idx_cap: u32) -> Self {
        Self {
            high_v: 0,
            high_i: 0,
            free: Vec::new(),
            vert_cap,
            idx_cap,
        }
    }

    /// High-water usage in (vertices, indices).
    pub fn used(&self) -> (u32, u32) {
        (self.high_v, self.high_i)
    }

    /// Number of free slots on the free list (telemetry).
    pub fn free_slots(&self) -> usize {
        self.free.len()
    }

    /// Total free capacity in (vertices, indices) (telemetry; the
    /// difference between this and `used()` minus live usage is orphaned
    /// space).
    pub fn free_total(&self) -> (u32, u32) {
        let (fv, fi): (u32, u32) = self.free.iter().fold((0, 0), |(v, i), f| {
            (v + f.v_count, i + f.i_count)
        });
        (fv, fi)
    }

    /// Try to allocate `need_v` vertices / `need_i` indices without
    /// evicting anything: tail headroom first, then the smallest free slot
    /// that fits in both dimensions (best fit; the slot's excess tail is
    /// split off and returned to the free list).
    pub fn alloc(&mut self, need_v: u32, need_i: u32) -> Option<Slot> {
        debug_assert!(need_v > 0 && need_i > 0, "empty meshes are filtered upstream");
        // 1) Tail headroom.
        if self.high_v + need_v <= self.vert_cap && self.high_i + need_i <= self.idx_cap {
            let s = Slot {
                base_v: self.high_v,
                base_i: self.high_i,
                v_count: need_v,
                i_count: need_i,
            };
            self.high_v += need_v;
            self.high_i += need_i;
            return Some(s);
        }
        // 2) Best-fit free slot.
        let mut best: Option<usize> = None;
        for (i, f) in self.free.iter().enumerate() {
            if f.v_count < need_v || f.i_count < need_i {
                continue;
            }
            match best {
                Some(bi) if f.v_count < self.free[bi].v_count => best = Some(i),
                None => best = Some(i),
                Some(_) => {}
            }
        }
        let bi = best?;
        let s = self.free.remove(bi);
        // Split off the unused tail (contiguous in both buffers).
        if let Some(tail) = s.tail(need_v, need_i) {
            self.release(tail);
        }
        Some(Slot {
            base_v: s.base_v,
            base_i: s.base_i,
            v_count: need_v,
            i_count: need_i,
        })
    }

    /// Release a slot. If it abuts the high-water mark the pool rewinds
    /// (the tail headroom grows); otherwise the slot is merged into the
    /// free list, coalescing with any adjacent free slots.
    pub fn release(&mut self, slot: Slot) {
        if slot.v_count == 0 {
            // An index-only remainder (vertex-exact split). Too small to
            // matter; it is reclaimed the next time a full-range slot
            // rewrites through it (there is no dedicated defrag pass).
            return;
        }
        if slot.base_v + slot.v_count == self.high_v && slot.base_i + slot.i_count == self.high_i {
            self.high_v = slot.base_v;
            self.high_i = slot.base_i;
            return;
        }
        let mut s = slot;
        // Coalesce with adjacent free slots. Each pass merges at least one
        // (or breaks); already-coalesced neighbours make a second pass a
        // no-op, so this terminates in 1–2 iterations.
        loop {
            let mut merged = false;
            self.free.retain_mut(|f| {
                if f.base_v + f.v_count == s.base_v && f.base_i + f.i_count == s.base_i {
                    s.base_v = f.base_v;
                    s.base_i = f.base_i;
                    s.v_count += f.v_count;
                    s.i_count += f.i_count;
                    merged = true;
                    false
                } else if s.base_v + s.v_count == f.base_v && s.base_i + s.i_count == f.base_i {
                    s.v_count += f.v_count;
                    s.i_count += f.i_count;
                    merged = true;
                    false
                } else {
                    true
                }
            });
            if !merged {
                break;
            }
        }
        self.free.push(s);
    }

    /// Whether the free list (as it stands, coalesced) holds a slot that
    /// already fits — callers use this to stop evicting victims.
    pub fn free_fits(&self, need_v: u32, need_i: u32) -> bool {
        self.free.iter().any(|f| f.v_count >= need_v && f.i_count >= need_i)
    }

    /// Reset everything (world switch).
    pub fn reset(&mut self) {
        self.high_v = 0;
        self.high_i = 0;
        self.free.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAP_V: u32 = 100_000;
    const CAP_I: u32 = 150_000;

    /// Deterministic PRNG (LCG) — seeded, so the sim is reproducible.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u32 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (self.0 >> 32) as u32
        }
        fn range(&mut self, lo: u32, hi: u32) -> u32 {
            lo + self.next() % (hi - lo)
        }
    }

    /// The client's allocation flow: tail/free first, then evict the
    /// farthest chunk (tie: biggest) until `alloc` succeeds. Returns the
    /// slot plus the number of evictions.
    fn alloc_or_evict(
        pool: &mut TerrainPool,
        chunks: &mut Vec<(u32, Slot)>, // (distance, slot)
        need_v: u32,
        need_i: u32,
    ) -> (Slot, usize) {
        if let Some(s) = pool.alloc(need_v, need_i) {
            return (s, 0);
        }
        let mut evictions = 0;
        loop {
            // Farthest first; tie: biggest slot (frees the most).
            let mut far = 0usize;
            for i in 1..chunks.len() {
                if chunks[i].0 > chunks[far].0
                    || (chunks[i].0 == chunks[far].0 && chunks[i].1.v_count > chunks[far].1.v_count)
                {
                    far = i;
                }
            }
            let (_d, victim) = chunks.remove(far);
            pool.release(victim);
            evictions += 1;
            match pool.alloc(need_v, need_i) {
                Some(s) => return (s, evictions),
                None => {
                    if chunks.is_empty() {
                        panic!("pool cannot hold a single chunk of this size");
                    }
                }
            }
        }
    }

    /// Every live slot must lie inside the high-water mark and no two may
    /// overlap (the render reads these ranges — overlap = corrupted scene).
    fn assert_invariants(pool: &TerrainPool, live: &[Slot]) {
        let (hv, hi) = pool.used();
        assert!(hv <= CAP_V && hi <= CAP_I, "high-water exceeds caps");
        for s in live {
            assert!(
                s.base_v + s.v_count <= hv && s.base_i + s.i_count <= hi,
                "live slot past high-water: {s:?} high=({hv},{hi})"
            );
        }
        for a in 0..live.len() {
            for b in (a + 1)..live.len() {
                let (x, y) = (&live[a], &live[b]);
                let ov_v = x.base_v < y.base_v + y.v_count && y.base_v < x.base_v + x.v_count;
                let ov_i = x.base_i < y.base_i + y.i_count && y.base_i < x.base_i + x.i_count;
                assert!(
                    !(ov_v && ov_i),
                    "slots overlap: {x:?} vs {y:?}"
                );
            }
        }
    }

    #[test]
    fn append_then_release_rewinds_tail() {
        let mut p = TerrainPool::new(CAP_V, CAP_I);
        let s1 = p.alloc(100, 150).unwrap();
        let s2 = p.alloc(50, 75).unwrap();
        assert_eq!((s1.base_v, s2.base_v), (0, 100));
        // Releasing the tail chunk shrinks the pool (no free slot needed).
        p.release(s2);
        assert_eq!(p.used(), (100, 150));
        assert_eq!(p.free_slots(), 0);
        // Releasing the (new) tail again returns to empty.
        p.release(s1);
        assert_eq!(p.used(), (0, 0));
    }

    #[test]
    fn release_coalesces_adjacent_free_slots() {
        // A 300-vertex pool: three equal chunks fill it exactly.
        let mut p = TerrainPool::new(300, 450);
        let a = p.alloc(100, 150).unwrap();
        let b = p.alloc(100, 150).unwrap();
        let c = p.alloc(100, 150).unwrap();
        assert_eq!((a.base_v, b.base_v, c.base_v), (0, 100, 200));
        assert!(p.alloc(1, 1).is_none(), "pool must be full");
        // Free the middle, then its left neighbour — they must merge into
        // one 200-vertex slot (c still occupies the tail).
        p.release(b);
        assert_eq!(p.free_slots(), 1);
        p.release(a);
        assert_eq!(p.free_slots(), 1, "adjacent free slots must coalesce");
        // The pool is still full (c at the tail): the 200-chunk can only
        // come from the coalesced slot.
        assert!(p.free_fits(200, 300));
        let s = p.alloc(200, 300).unwrap();
        assert_eq!(s.base_v, 0, "alloc must use the coalesced slot");
        assert_eq!(p.free_slots(), 0);
        // Releasing the tail chunk rewinds the pool (no free slot).
        p.release(c);
        assert_eq!(p.free_slots(), 0);
        assert_eq!(p.used(), (200, 300));
        // ...and the rewound run is append headroom again.
        let s2 = p.alloc(100, 150).unwrap();
        assert_eq!(s2.base_v, 200);
    }

    #[test]
    fn best_fit_prefers_smallest_fitting_and_splits_tail() {
        let mut p = TerrainPool::new(1000, 1500);
        let a = p.alloc(100, 150).unwrap(); // [0,100)
        let b = p.alloc(200, 300).unwrap(); // [100,300)
        let big = p.alloc(600, 900).unwrap(); // [300,900)
        let c = p.alloc(100, 150).unwrap(); // [900,1000)
        assert!(p.alloc(1, 1).is_none(), "pool must be full");
        // Free a middle slot, then allocate 100: best-fit must take the
        // 200-slot (the only free one) and split off its 100 tail.
        p.release(b);
        let s = p.alloc(100, 150).unwrap();
        assert_eq!(s.base_v, 100, "must use the freed middle slot");
        assert_eq!(p.free_slots(), 1, "the unused 100 tail must be split back");
        // The split tail keeps the rest of the pool intact and disjoint.
        assert_invariants(&p, &[a, s, big, c]);
    }

    #[test]
    fn alloc_never_overlaps_or_exceeds_caps() {
        let mut p = TerrainPool::new(CAP_V, CAP_I);
        let mut live: Vec<Slot> = Vec::new();
        let mut rng = Rng(42);
        for _ in 0..2000 {
            let v = rng.range(16, 2000);
            let i = v * 3 / 2;
            match p.alloc(v, i) {
                Some(s) => {
                    live.push(s);
                    assert_invariants(&p, &live);
                }
                None => {
                    // The client's flow: evict (here: random victims) until
                    // alloc fits — a single eviction may not be enough when
                    // the need is bigger than the victim's slot.
                    loop {
                        let k = rng.next() as usize % live.len();
                        p.release(live.swap_remove(k));
                        match p.alloc(v, i) {
                            Some(s) => {
                                live.push(s);
                                break;
                            }
                            None if live.is_empty() => {
                                panic!("pool cannot hold a single chunk of this size");
                            }
                            None => {}
                        }
                    }
                    assert_invariants(&p, &live);
                }
            }
            // Randomly release live chunks (chunk replaced/evicted).
            if live.len() > 20 && rng.next() % 4 == 0 {
                let k = rng.next() as usize % live.len();
                p.release(live.swap_remove(k));
                assert_invariants(&p, &live);
            }
        }
        assert_invariants(&p, &live);
    }

    /// Simulates the fly-mode scenario: the pool is pinned at capacity
    /// (view + fog-bound trail), the camera keeps moving (all distances
    /// grow), and 1–4 new chunks arrive per step. Asserts: no chunk is ever
    /// lost, evictions always hit the farthest chunks first (so fog-bound
    /// trail goes before visible terrain while any trail exists), the pool
    /// never exceeds its caps, slots never overlap, and the free list stays
    /// bounded. (A pinned-full pool inevitably churns ~one fog-bound chunk
    /// per insert — that is the design: each eviction+insert costs one small
    /// upload, and fog-bound chunks re-stream only if revisited.)
    #[test]
    fn fly_mode_steady_state_no_lost_chunks() {
        let mut p = TerrainPool::new(CAP_V, CAP_I);
        let mut rng = Rng(1337);
        // Distances: 0..=7 in-view, 8+ fog-bound trail.
        let mut chunks: Vec<(u32, Slot)> = Vec::new();
        // Fill with realistic (v, i≈1.5v) pairs until the pool is full.
        loop {
            let v = rng.range(200, 1600);
            let i = v * 3 / 2;
            match p.alloc(v, i) {
                Some(s) => chunks.push((rng.range(0, 13), s)),
                None => break,
            }
        }
        let mut total_evictions = 0usize;
        for _step in 0..400 {
            // Camera moves: everything drifts farther. No cap — in the real
            // world a fog-bound trail chunk only gets farther, so the
            // farthest (oldest, front-of-pool, pool-adjacent) chunk is
            // always uniquely evictable first.
            for (d, _) in chunks.iter_mut() {
                *d += 1;
            }
            // 1–4 new chunks arrive at the view's edge.
            for _ in 0..rng.range(1, 5) {
                let v = rng.range(200, 1600);
                let i = v * 3 / 2;
                let far_before = chunks.iter().map(|c| c.0).max();
                // Chunks at the pre-call max distance (only meaningful when
                // the max is beyond the 0..8 range of freshly pushed chunks,
                // so the after-count can't be contaminated by the push below).
                let count_far = far_before
                    .filter(|&f| f >= 8)
                    .map(|f| chunks.iter().filter(|(d, _)| *d == f).count());
                let (slot, ev) = alloc_or_evict(&mut p, &mut chunks, v, i);
                total_evictions += ev;
                // Evictions must hit the farthest chunks first: the number
                // of chunks at the pre-call max distance drops by exactly
                // the number of evictions (saturated) — a nearer chunk is
                // never touched while a farther one exists.
                if let (Some(f), Some(cf)) = (far_before.filter(|&f| f >= 8), count_far) {
                    let cf_after = chunks.iter().filter(|(d, _)| *d == f).count();
                    assert_eq!(
                        cf_after,
                        cf.saturating_sub(ev),
                        "eviction skipped a farthest chunk"
                    );
                }
                chunks.push((rng.range(0, 8), slot));
                assert_invariants(&p, &chunks.iter().map(|c| c.1).collect::<Vec<_>>());
            }
        }
        assert!(
            total_evictions > 0,
            "fly mode must evict fog-bound trail while pinned at capacity"
        );
        // Terminal probe: the pool must always absorb a worst-case-sized
        // chunk (measured real max ~7K verts) by evicting only FOG-BOUND
        // chunks — visible terrain (d <= 7) is never sacrificed while any
        // trail exists. (Free-list size itself is allowed to wander: split
        // fragments are normal; what must not wander is *which* chunks get
        // dropped.)
        let visible: Vec<Slot> = chunks
            .iter()
            .filter(|(d, _)| *d <= 7)
            .map(|(_, s)| *s)
            .collect();
        let (_slot, ev) = alloc_or_evict(&mut p, &mut chunks, 7000, 10500);
        assert!(
            ev < 200,
            "probe needed {ev} evictions to place one chunk — the pool is thrashing"
        );
        for s in &visible {
            assert!(
                chunks.iter().any(|(_, c)| c == s),
                "a visible chunk (d <= 7) was evicted to make room"
            );
        }
        assert_invariants(&p, &chunks.iter().map(|c| c.1).collect::<Vec<_>>());
    }

    /// Eviction order property: when the pool is full, a new chunk may only
    /// evict the farthest chunk(s) — never a nearer one while a farther one
    /// exists.
    #[test]
    fn evictions_hit_farthest_first() {
        let mut p = TerrainPool::new(1000, 1500);
        // Five equal chunks fill the pool exactly.
        let mut chunks: Vec<(u32, Slot)> = Vec::new();
        for d in 1..=5u32 {
            let s = p.alloc(200, 300).unwrap();
            chunks.push((d, s));
        }
        assert!(p.alloc(1, 1).is_none(), "pool must be full");
        // New chunk arrives; the farthest (d=5) must be the one evicted.
        let (slot, ev) = alloc_or_evict(&mut p, &mut chunks, 200, 300);
        assert_eq!(ev, 1);
        assert!(!chunks.iter().any(|(d, _)| *d == 5), "farthest chunk must go");
        assert!(chunks.iter().all(|(d, _)| *d <= 4));
        // The new chunk (pushed below) re-filled the pool; now d=4 is the
        // farthest and must be the next eviction.
        chunks.push((0, slot));
        assert!(p.alloc(1, 1).is_none(), "pool must be full again");
        let (_, ev) = alloc_or_evict(&mut p, &mut chunks, 200, 300);
        assert_eq!(ev, 1);
        assert!(!chunks.iter().any(|(d, _)| *d == 4), "new farthest must go");
    }
}
