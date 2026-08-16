use super::*;

impl<S: NodeStore> BeTree<S> {
    // ---------------------------------------------------------------- cursors

    /// An ordered cursor over one root, restricted to `[lo, hi)`.
    fn cursor(&self, root: BlockId, lo: Option<Bytes>, hi: Option<Bytes>) -> Cursor<'_, S> {
        self.cursor_over(root, vec![KeyRange::new(lo, hi)])
    }

    /// A cursor restricted to a *set* of ranges. Children disjoint from every range are pruned, so many
    /// scattered windows still cost O(depth) waves over their union rather than one walk per window —
    /// and, crucially, not a walk of everything between the lowest and highest window.
    ///
    /// `ranges` need not be sorted or disjoint; it is normalized here.
    pub(super) fn cursor_over(&self, root: BlockId, ranges: Vec<KeyRange>) -> Cursor<'_, S> {
        let ranges = normalize_ranges(ranges);
        let span = KeyRange::new(
            ranges.first().and_then(|range| range.lo.clone()),
            ranges.last().and_then(|range| range.hi.clone()),
        );
        Cursor {
            tree: self,
            frames: Vec::new(),
            pending: (!ranges.is_empty()).then_some(Pending {
                id: root,
                range: span.clone(),
                expect_level: None,
            }),
            batch: Vec::new().into_iter(),
            span,
            ranges,
            prefetch_width: DEFAULT_PREFETCH_WIDTH,
            visited_nodes: 0,
        }
    }

    pub(super) async fn live_pairs(
        &self,
        rows: Vec<(Bytes, Winner)>,
        budget: &mut BudgetState,
    ) -> Result<Vec<(Bytes, Bytes)>, TreeError> {
        let (keys, winners): (Vec<Bytes>, Vec<Option<Winner>>) =
            rows.into_iter().map(|(k, w)| (k, Some(w))).unzip();
        let values = self.materialize(winners, budget).await?;
        Ok(keys
            .into_iter()
            .zip(values)
            .filter_map(|(k, v)| v.map(|v| (k, v)))
            .collect())
    }
}

/// Multi-root scans support sharded or independently partitioned datasets through one merged scan.
///
/// Roots are expected to own disjoint key spaces, so this is a *merge* rather than an arbitration — but
/// when two roots do hold the same key, the greater winner tuple wins, exactly as it would inside one
/// tree. That keeps the answer well defined instead of order dependent.
impl<S: NodeStore> BeTree<S> {
    async fn collect_roots(
        &self,
        roots: &[BlockId],
        ranges: Vec<KeyRange>,
        budget: &mut BudgetState,
    ) -> Result<Vec<(Bytes, Winner)>, TreeError> {
        let mut acc: BTreeMap<Bytes, Winner> = BTreeMap::new();
        // Distinct roots only: folding one root twice would double-visit its entries.
        let mut seen: foldhash::HashSet<BlockId> = Default::default();
        for root in roots.iter().copied().filter(|r| seen.insert(*r)) {
            let mut c = self.cursor_over(root, ranges.clone());
            while let Some((k, w)) = c.next_kv(budget).await? {
                match acc.get_mut(&k) {
                    Some(cur) if *cur >= w => {}
                    Some(cur) => *cur = w,
                    None => {
                        acc.insert(k, w);
                    }
                }
            }
        }
        Ok(acc.into_iter().collect())
    }

    /// Half-open range scan across many roots.
    pub async fn scan_range_roots(
        &self,
        roots: &[BlockId],
        lo: Option<&[u8]>,
        hi: Option<&[u8]>,
    ) -> Result<Vec<(Bytes, Bytes)>, TreeError> {
        let mut budget = BudgetState::new(self.budget);
        let ranges = vec![KeyRange::new(
            lo.map(Bytes::copy_from_slice),
            hi.map(Bytes::copy_from_slice),
        )];
        let rows = self.collect_roots(roots, ranges, &mut budget).await?;
        self.live_pairs(rows, &mut budget).await
    }

    /// Prefix scan across many roots.
    pub async fn scan_prefix_roots(
        &self,
        roots: &[BlockId],
        prefix: &[u8],
    ) -> Result<Vec<(Bytes, Bytes)>, TreeError> {
        let hi = prefix_succ(prefix);
        self.scan_range_roots(roots, Some(prefix), hi.as_deref())
            .await
    }

    /// Many prefixes across many roots, in one pruned walk per root. One live list per input prefix, in
    /// input order.
    pub async fn scan_prefix_many_roots(
        &self,
        roots: &[BlockId],
        prefixes: &[&[u8]],
    ) -> Result<Vec<Vec<(Bytes, Bytes)>>, TreeError> {
        if prefixes.is_empty() || roots.is_empty() {
            return Ok(vec![Vec::new(); prefixes.len()]);
        }
        let mut budget = BudgetState::new(self.budget);
        let ranges: Vec<KeyRange> = prefixes
            .iter()
            .map(|p| KeyRange::new(Some(Bytes::copy_from_slice(p)), prefix_succ(p)))
            .collect();
        let rows = self.collect_roots(roots, ranges, &mut budget).await?;
        let live = self.live_pairs(rows, &mut budget).await?;
        Ok(bucket_by_prefix(prefixes, live))
    }
}

/// Route each live row into every input prefix it matches, by one hash lookup per distinct prefix
/// LENGTH. Shared by the single- and multi-root prefix-many paths so they cannot diverge.
pub(super) fn bucket_by_prefix(
    prefixes: &[&[u8]],
    live: Vec<(Bytes, Bytes)>,
) -> Vec<Vec<(Bytes, Bytes)>> {
    let mut by_len: foldhash::HashMap<usize, foldhash::HashMap<&[u8], Vec<usize>>> =
        Default::default();
    for (i, p) in prefixes.iter().enumerate() {
        by_len
            .entry(p.len())
            .or_default()
            .entry(*p)
            .or_default()
            .push(i);
    }
    let mut out: Vec<Vec<(Bytes, Bytes)>> = vec![Vec::new(); prefixes.len()];
    for (k, v) in live {
        for (len, index) in &by_len {
            if k.len() < *len {
                continue;
            }
            if let Some(slots) = index.get(&k[..*len]) {
                for &i in slots {
                    out[i].push((k.clone(), v.clone()));
                }
            }
        }
    }
    out
}

/// The exclusive successor of a prefix (increment with carry, truncating trailing `0xFF`). `None` means
/// the prefix is all-`0xFF`, so it is unbounded above.
pub(super) fn prefix_succ(prefix: &[u8]) -> Option<Bytes> {
    let mut hi = prefix.to_vec();
    for i in (0..hi.len()).rev() {
        if hi[i] != 0xFF {
            hi[i] += 1;
            hi.truncate(i + 1);
            return Some(Bytes::from(hi));
        }
    }
    None
}

/// One half-open key interval. Bounds travel together so callers cannot accidentally intersect a
/// lower bound from one subtree with an upper bound from another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct KeyRange {
    lo: Option<Bytes>,
    hi: Option<Bytes>,
}

impl KeyRange {
    pub(super) fn new(lo: Option<Bytes>, hi: Option<Bytes>) -> Self {
        Self { lo, hi }
    }

    fn contains(&self, key: &[u8]) -> bool {
        self.lo.as_ref().is_none_or(|lo| key >= lo.as_ref())
            && self.hi.as_ref().is_none_or(|hi| key < hi.as_ref())
    }

    fn is_empty(&self) -> bool {
        matches!((&self.lo, &self.hi), (Some(lo), Some(hi)) if lo >= hi)
    }

    fn intersects(&self, other: &Self) -> bool {
        !matches!((&self.hi, &other.lo), (Some(hi), Some(lo)) if hi <= lo)
            && !matches!((&self.lo, &other.hi), (Some(lo), Some(hi)) if lo >= hi)
    }

    fn intersection(&self, other: &Self) -> Self {
        let lo = match (&self.lo, &other.lo) {
            (Some(a), Some(b)) => Some(a.max(b).clone()),
            (Some(bound), None) | (None, Some(bound)) => Some(bound.clone()),
            (None, None) => None,
        };
        let hi = match (&self.hi, &other.hi) {
            (Some(a), Some(b)) => Some(a.min(b).clone()),
            (Some(bound), None) | (None, Some(bound)) => Some(bound.clone()),
            (None, None) => None,
        };
        Self { lo, hi }
    }

    /// Extend this range upward through an overlapping or touching successor.
    fn merge(&mut self, other: Self) {
        self.hi = match (&self.hi, other.hi) {
            (Some(a), Some(b)) => Some(a.max(&b).clone()),
            _ => None,
        };
    }
}

/// Sort ranges by lower bound and coalesce every overlapping or touching pair, dropping empty ones. The
/// result is sorted and pairwise disjoint, which is what makes the cursor's overlap test a binary search
/// instead of a scan.
fn normalize_ranges(mut ranges: Vec<KeyRange>) -> Vec<KeyRange> {
    // `None` as a lower bound sorts first; `None` as an upper bound sorts last.
    ranges.retain(|range| !range.is_empty());
    ranges.sort_by(|a, b| match (&a.lo, &b.lo) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Less,
        (Some(_), None) => std::cmp::Ordering::Greater,
        (Some(x), Some(y)) => x.cmp(y),
    });
    let mut out: Vec<KeyRange> = Vec::with_capacity(ranges.len());
    for range in ranges {
        match out.last_mut() {
            // The next range starts at or before the current one ends, so they merge.
            Some(previous)
                if previous.hi.is_none()
                    || range
                        .lo
                        .as_ref()
                        .is_none_or(|lo| previous.hi.as_ref().is_some_and(|hi| lo <= hi)) =>
            {
                previous.merge(range);
            }
            _ => out.push(range),
        }
    }
    out
}

/// A subtree the cursor is positioned at but has not entered.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Pending {
    id: BlockId,
    range: KeyRange,
    expect_level: Option<u16>,
}

struct Frame {
    view: Arc<NodeView>,
    /// Absolute key range this node owns, already intersected with the scan range.
    range: KeyRange,
    next_child: usize,
    /// Children of this frame actually entered, and children skipped without being read.
    entered: usize,
    skipped: usize,
    /// Whether this frame's remaining in-scope children have already been prefetched.
    prefetched: bool,
}

/// An ordered cursor: it yields resolved `(key, winner)` pairs in key order, and it exposes the
/// *ancestor buffer overlays* covering the range it is about to enter.
///
/// That overlay state is what makes it correct for a buffered tree. Copying a prolly-tree cursor
/// without it would be wrong: an equal subtree id proves the subtree's own entries match, but a
/// newer message sitting in an ancestor buffer above it can still change the resolved winner.
pub(super) struct Cursor<'t, S: NodeStore> {
    tree: &'t BeTree<S>,
    frames: Vec<Frame>,
    pending: Option<Pending>,
    batch: std::vec::IntoIter<(Bytes, Winner)>,
    /// The union's outer bounds, used to bound one leaf's entry sweep.
    span: KeyRange,
    /// Sorted, pairwise disjoint ranges. A key is in scope iff it falls in one of them.
    ranges: Vec<KeyRange>,
    /// Objects one lookahead may fetch. Bounds the speculative memory a scan pulls into the cache.
    prefetch_width: usize,
    visited_nodes: u64,
}

pub(super) type RangeSet = [KeyRange];

/// Does `[lo, hi)` overlap any range in the normalized set? A binary search, so pruning never scales
/// with the number of ranges.
pub(super) fn overlaps_scope(ranges: &RangeSet, candidate: &KeyRange) -> bool {
    if ranges.len() == 1 {
        return candidate.intersects(&ranges[0]);
    }
    // First range whose lower bound is not below `hi`; the candidate is that one or its predecessor.
    let at = ranges.partition_point(|range| match (&range.lo, &candidate.hi) {
        (_, None) => true,
        (None, Some(_)) => true,
        (Some(rlo), Some(hi)) => rlo < hi,
    });
    (at.saturating_sub(1)..=at)
        .filter_map(|i| ranges.get(i))
        .any(|range| candidate.intersects(range))
}

/// Is `key` inside one of the ranges?
fn in_scope(ranges: &RangeSet, key: &[u8]) -> bool {
    if ranges.len() == 1 {
        return ranges[0].contains(key);
    }
    let at = ranges.partition_point(|range| range.lo.as_ref().is_none_or(|lo| lo.as_ref() <= key));
    at.checked_sub(1)
        .and_then(|i| ranges.get(i))
        .is_some_and(|range| range.contains(key))
}

/// Merge two sorted, unique winner runs. A leaf and its collapsed ancestor overlays already have the
/// order a cursor needs; rebuilding that order through a tree map adds O(n log n) comparisons and one
/// allocation per key.
fn merge_winner_runs(leaf: Vec<(Bytes, Winner)>, overlays: Vec<Entry>) -> Vec<(Bytes, Winner)> {
    let capacity = leaf.len().saturating_add(overlays.len());
    let mut leaf = leaf.into_iter().peekable();
    let mut overlays = overlays
        .into_iter()
        .map(|entry| {
            let winner = entry.winner();
            (entry.key, winner)
        })
        .peekable();
    let mut merged = Vec::with_capacity(capacity);
    loop {
        match (leaf.peek(), overlays.peek()) {
            (Some((leaf_key, _)), Some((overlay_key, _))) => {
                use std::cmp::Ordering;
                match leaf_key.cmp(overlay_key) {
                    Ordering::Less => merged.push(leaf.next().expect("peeked")),
                    Ordering::Greater => merged.push(overlays.next().expect("peeked")),
                    Ordering::Equal => {
                        let (key, leaf_winner) = leaf.next().expect("peeked");
                        let (_, overlay_winner) = overlays.next().expect("peeked");
                        merged.push((key, leaf_winner.max(overlay_winner)));
                    }
                }
            }
            (Some(_), None) => {
                merged.extend(leaf);
                break;
            }
            (None, Some(_)) => {
                merged.extend(overlays);
                break;
            }
            (None, None) => break,
        }
    }
    merged
}

impl<'t, S: NodeStore> Cursor<'t, S> {
    /// The subtree about to be entered, if the cursor is at a subtree boundary with nothing buffered.
    fn peek_pending(&self) -> Option<&Pending> {
        self.batch
            .as_slice()
            .is_empty()
            .then_some(self.pending.as_ref())
            .flatten()
    }

    fn visited_nodes(&self) -> u64 {
        self.visited_nodes
    }

    /// Merge the ancestor buffer runs overlapping `[lo, hi)` into one entry per key, keeping the greater
    /// winner. Depth is not authority: the winner tuple decides.
    fn overlays_in(&self, range: &KeyRange) -> Vec<Entry> {
        let mut acc: BTreeMap<Bytes, Entry> = BTreeMap::new();
        for f in &self.frames {
            let v = &f.view;
            let start = match &range.lo {
                Some(l) => v.entry_surface().probe(l).index,
                None => 0,
            };
            for i in start..v.entry_count() {
                let key = v.entry_key(i);
                if range.hi.as_ref().is_some_and(|hi| key >= hi.as_ref()) {
                    break;
                }
                let e = v.entry(i);
                match acc.get_mut(&e.key) {
                    Some(cur) if cur.winner() >= e.winner() => {}
                    slot => {
                        let key = e.key.clone();
                        match slot {
                            Some(cur) => *cur = e,
                            None => {
                                acc.insert(key, e);
                            }
                        }
                    }
                }
            }
        }
        acc.into_values().collect()
    }

    /// Discard the pending subtree without loading it. Used only when the caller has already accounted
    /// for that range some other way.
    fn skip_pending(&mut self) {
        self.pending = None;
        if let Some(f) = self.frames.last_mut() {
            f.skipped += 1;
        }
        self.advance();
    }

    /// The remaining in-scope children of the deepest frame, for one batched prefetch.
    fn sibling_run(&self) -> Vec<BlockId> {
        let Some(f) = self.frames.last() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        // `next_child` already points past the pending child, so include the pending explicitly.
        if let Some(p) = &self.pending {
            out.push(p.id);
        }
        for i in f.next_child..f.view.child_count() {
            let (clo, chi) = f.view.child_range(i);
            let child = KeyRange::new(
                clo.map(Bytes::copy_from_slice),
                chi.map(Bytes::copy_from_slice),
            );
            let range = f.range.intersection(&child);
            if overlaps_scope(&self.ranges, &range) {
                out.push(f.view.child(i));
            }
        }
        out
    }

    /// Set `pending` to the next child to visit, popping exhausted frames. Children whose range is
    /// disjoint from the scan range are pruned here.
    fn advance(&mut self) {
        while let Some(f) = self.frames.last_mut() {
            if f.next_child >= f.view.child_count() {
                self.frames.pop();
                continue;
            }
            let i = f.next_child;
            f.next_child += 1;
            let (clo, chi) = f.view.child_range(i);
            let child = KeyRange::new(
                clo.map(Bytes::copy_from_slice),
                chi.map(Bytes::copy_from_slice),
            );
            let range = f.range.intersection(&child);
            if !overlaps_scope(&self.ranges, &range) {
                continue;
            }
            self.pending = Some(Pending {
                id: f.view.child(i),
                range,
                // A malformed level-zero internal node must remain a typed error path rather than
                // underflowing while constructing the next cursor frame. The subsequent load check
                // still validates the edge; saturating here only protects the verifier-off path.
                expect_level: Some(f.view.tree_level().saturating_sub(1)),
            });
            return;
        }
        self.pending = None;
    }

    /// Descend until a leaf is reached, then fill `batch` with that leaf's range, resolved against the
    /// ancestor overlays. A batch is bounded by one leaf, so memory is bounded by `NODE_BYTES`.
    async fn fill(&mut self, budget: &mut BudgetState) -> Result<(), TreeError> {
        while self.batch.as_slice().is_empty() {
            let Some(p) = self.pending.take() else {
                return Ok(());
            };
            // Cursors are ordered walks: their leaf level is sequential, their spine is metadata.
            let hint = BeTree::<S>::descent_hint(p.expect_level, true);
            // A prefetch charges bytes but not object visits; the visit is charged by the load below.

            // Batch the whole remaining sibling run in ONE read rather than one `get` per node. A serial
            // cursor turned a cold 4 000-key scan into 337 scalar gets and zero batched reads.
            //
            // The guard is what keeps this from hurting `diff`: a frame that has *skipped* a child is one
            // whose subtrees the caller intends not to read (an equal-subtree skip), so prefetching them
            // would fetch exactly what the skip exists to avoid. A scan never skips, so it always
            // batches; an aligned diff skips before its first load, so it never does.
            let should_prefetch = self
                .frames
                .last()
                .is_some_and(|f| !f.prefetched && f.skipped == 0);
            if should_prefetch {
                let run = self.sibling_run();
                if run.len() > 1 {
                    let ranges = self.ranges.clone();
                    self.tree
                        .prefetch_ahead(run, &ranges, self.prefetch_width, budget)
                        .await;
                }
                if let Some(f) = self.frames.last_mut() {
                    f.prefetched = true;
                }
            }

            let view = self.tree.load(p.id, hint, budget).await?;
            self.visited_nodes += 1;
            if let Some(f) = self.frames.last_mut() {
                f.entered += 1;
            }
            if let Some(expect) = p.expect_level
                && view.tree_level() != expect
            {
                return Err(TreeError::decode(
                    Some(p.id),
                    DecodeError::ChildLevel {
                        child: view.tree_level(),
                        parent: expect + 1,
                    },
                ));
            }
            if view.is_leaf() {
                let range = p.range.intersection(&self.span);
                // k-way merge of this leaf's run with every overlapping ancestor buffer run. The
                // greatest winner tuple wins, so traversal-source priority has no semantic role.
                let mut leaf = Vec::with_capacity(view.entry_count());
                let start = match &range.lo {
                    Some(l) => view.entry_surface().probe(l).index,
                    None => 0,
                };
                for i in start..view.entry_count() {
                    let key = view.entry_key(i);
                    if range.hi.as_ref().is_some_and(|hi| key >= hi.as_ref()) {
                        break;
                    }
                    if !in_scope(&self.ranges, key) {
                        continue;
                    }
                    leaf.push((view.entry_key_bytes(i), view.winner(i)));
                }
                let overlays = self
                    .overlays_in(&range)
                    .into_iter()
                    .filter(|entry| in_scope(&self.ranges, &entry.key))
                    .collect();
                self.batch = merge_winner_runs(leaf, overlays).into_iter();
                self.advance();
            } else {
                let range = p.range.intersection(&self.span);
                self.frames.push(Frame {
                    view,
                    range,
                    next_child: 0,
                    entered: 0,
                    skipped: 0,
                    prefetched: false,
                });
                // Fresh frame: its first in-range child becomes the new pending.
                self.advance();
            }
        }
        Ok(())
    }

    async fn peek_kv(
        &mut self,
        budget: &mut BudgetState,
    ) -> Result<Option<&(Bytes, Winner)>, TreeError> {
        self.fill(budget).await?;
        Ok(self.batch.as_slice().first())
    }

    pub(super) async fn next_kv(
        &mut self,
        budget: &mut BudgetState,
    ) -> Result<Option<(Bytes, Winner)>, TreeError> {
        self.fill(budget).await?;
        Ok(self.batch.next())
    }
}

/// A backpressured live range scan. One call advances at most to the next live row; memory is bounded
/// by one decoded leaf, its ancestor overlays, and the cursor's bounded prefetch window.
pub struct ScanCursor<'t, S: NodeStore> {
    tree: &'t BeTree<S>,
    inner: Cursor<'t, S>,
    budget: BudgetState,
}

impl<'t, S: NodeStore> ScanCursor<'t, S> {
    /// Set the maximum number of node objects fetched by one breadth-first lookahead. A width of zero
    /// disables lookahead; result order and exactness are independent of this throughput knob.
    pub fn with_prefetch_width(mut self, width: usize) -> Self {
        self.inner.prefetch_width = width;
        self
    }

    pub async fn next(&mut self) -> Result<Option<(Bytes, Bytes)>, TreeError> {
        Ok(self.next_batch(1).await?.pop())
    }

    /// Pull at most `max_rows` raw winners and materialize all external values in one coalesced wave.
    /// This is the throughput form of the cursor: bounded memory and backpressure without turning an
    /// external-value scan into one round trip per row.
    pub async fn next_batch(&mut self, max_rows: usize) -> Result<Vec<(Bytes, Bytes)>, TreeError> {
        if max_rows == 0 {
            return Ok(Vec::new());
        }
        let mut rows = Vec::with_capacity(max_rows);
        while rows.len() < max_rows {
            let Some((key, winner)) = self.inner.next_kv(&mut self.budget).await? else {
                break;
            };
            if !matches!(winner.op, WinnerOp::Tombstone) {
                rows.push((key, winner));
            }
        }
        self.tree.live_pairs(rows, &mut self.budget).await
    }
}

/// A backpressured Merkle diff. Equal pending subtrees are skipped before any bytes below them are
/// loaded; ancestor-overlay candidates for that exact range are resolved as one small batch.
pub struct DiffCursor<'t, S: NodeStore> {
    tree: &'t BeTree<S>,
    a_root: BlockId,
    b_root: BlockId,
    a: Cursor<'t, S>,
    b: Cursor<'t, S>,
    budget: BudgetState,
    ready: VecDeque<Bytes>,
    done: bool,
    reported: bool,
}

impl<'t, S: NodeStore> DiffCursor<'t, S> {
    /// Set lookahead independently of result batch size. Equal subtrees are still skipped before
    /// prefetch, so a wider value cannot defeat Merkle pruning.
    pub fn with_prefetch_width(mut self, width: usize) -> Self {
        self.a.prefetch_width = width;
        self.b.prefetch_width = width;
        self
    }

    pub async fn next(&mut self) -> Result<Option<Bytes>, TreeError> {
        if let Some(key) = self.ready.pop_front() {
            return Ok(Some(key));
        }
        while !self.done {
            let equal = match (self.a.peek_pending(), self.b.peek_pending()) {
                (Some(a), Some(b)) if a.id == b.id && a.range == b.range => Some(a.range.clone()),
                _ => None,
            };
            if let Some(range) = equal {
                self.tree.metrics.diff_equal_id_skip();
                let oa = self.a.overlays_in(&range);
                let ob = self.b.overlays_in(&range);
                if oa != ob {
                    let keys: Vec<Bytes> = oa
                        .iter()
                        .chain(&ob)
                        .map(|entry| entry.key.clone())
                        .collect::<std::collections::BTreeSet<_>>()
                        .into_iter()
                        .collect();
                    if !keys.is_empty() {
                        let refs: Vec<&[u8]> = keys.iter().map(|key| key.as_ref()).collect();
                        let wa = self
                            .tree
                            .resolve_many(self.a_root, &refs, &mut self.budget)
                            .await?;
                        let wb = self
                            .tree
                            .resolve_many(self.b_root, &refs, &mut self.budget)
                            .await?;
                        self.ready
                            .extend(keys.into_iter().zip(wa).zip(wb).filter_map(
                                |((key, a), b)| (live_value(&a) != live_value(&b)).then_some(key),
                            ));
                    }
                }
                self.a.skip_pending();
                self.b.skip_pending();
                if let Some(key) = self.ready.pop_front() {
                    return Ok(Some(key));
                }
                continue;
            }

            let ka = self
                .a
                .peek_kv(&mut self.budget)
                .await?
                .map(|(key, _)| key.clone());
            let kb = self
                .b
                .peek_kv(&mut self.budget)
                .await?
                .map(|(key, _)| key.clone());
            let next = match (ka, kb) {
                (None, None) => {
                    if self.a.peek_pending().is_none() && self.b.peek_pending().is_none() {
                        self.done = true;
                    }
                    None
                }
                (Some(a), Some(b)) if a == b => {
                    let (_, wa) = self.a.next_kv(&mut self.budget).await?.expect("peeked");
                    let (_, wb) = self.b.next_kv(&mut self.budget).await?.expect("peeked");
                    self.tree.metrics.diff_visited_keys(2);
                    (observable(&wa) != observable(&wb)).then_some(a)
                }
                (Some(a), b) if b.as_ref().is_none_or(|b| a < *b) => {
                    let (key, winner) = self.a.next_kv(&mut self.budget).await?.expect("peeked");
                    self.tree.metrics.diff_visited_keys(1);
                    observable(&winner).is_some().then_some(key)
                }
                _ => {
                    let (key, winner) = self.b.next_kv(&mut self.budget).await?.expect("peeked");
                    self.tree.metrics.diff_visited_keys(1);
                    observable(&winner).is_some().then_some(key)
                }
            };
            if let Some(key) = next {
                return Ok(Some(key));
            }
        }
        if !self.reported {
            self.tree
                .metrics
                .diff_visited_nodes(self.a.visited_nodes() + self.b.visited_nodes());
            self.reported = true;
        }
        Ok(None)
    }

    /// Pull at most `max_keys` differences. Zero is a non-advancing request.
    pub async fn next_batch(&mut self, max_keys: usize) -> Result<Vec<Bytes>, TreeError> {
        let mut out = Vec::with_capacity(max_keys);
        while out.len() < max_keys {
            let Some(key) = self.next().await? else {
                break;
            };
            out.push(key);
        }
        Ok(out)
    }
}

impl<S: NodeStore> BeTree<S> {
    pub fn scan_cursor(
        &self,
        root: BlockId,
        lo: Option<&[u8]>,
        hi: Option<&[u8]>,
    ) -> ScanCursor<'_, S> {
        ScanCursor {
            tree: self,
            inner: self.cursor(
                root,
                lo.map(Bytes::copy_from_slice),
                hi.map(Bytes::copy_from_slice),
            ),
            budget: BudgetState::new(self.budget),
        }
    }

    /// Prefix form of [`Self::scan_cursor`], including the all-`0xff` unbounded upper edge.
    pub fn scan_prefix_cursor(&self, root: BlockId, prefix: &[u8]) -> ScanCursor<'_, S> {
        let hi = prefix_succ(prefix);
        ScanCursor {
            tree: self,
            inner: self.cursor(root, Some(Bytes::copy_from_slice(prefix)), hi),
            budget: BudgetState::new(self.budget),
        }
    }

    pub fn diff_cursor(&self, a: BlockId, b: BlockId) -> DiffCursor<'_, S> {
        DiffCursor {
            tree: self,
            a_root: a,
            b_root: b,
            a: self.cursor(a, None, None),
            b: self.cursor(b, None, None),
            budget: BudgetState::new(self.budget),
            ready: VecDeque::new(),
            done: a == b,
            reported: false,
        }
    }

    /// Rewrite every resolved winner from `source_root` into this tree's format. This is deliberately
    /// an offline streaming operation: it preserves tombstones and exact order keys, materializes old
    /// external values through the source decoder, bounds live memory by `batch_rows + depth`, and
    /// never mutates or deletes the historical object graph.
    pub async fn migrate_from<T: NodeStore>(
        &self,
        source: &BeTree<T>,
        source_root: BlockId,
        batch_rows: usize,
    ) -> Result<MigrationReport, TreeError> {
        if batch_rows == 0 {
            return Err(TreeError::Capacity(CapacityError::Format(
                "migration batch_rows must be nonzero".into(),
            )));
        }
        let mut target_root = self.empty_root().await?;
        let mut cursor = source.cursor(source_root, None, None);
        let mut source_budget = BudgetState::new(source.budget);
        let mut rows = 0u64;
        let mut apply_batches = 0u64;

        loop {
            let mut raw = Vec::with_capacity(batch_rows);
            while raw.len() < batch_rows {
                let Some(row) = cursor.next_kv(&mut source_budget).await? else {
                    break;
                };
                raw.push(row);
            }
            if raw.is_empty() {
                break;
            }

            let refs: Vec<(BlockId, u32)> = raw
                .iter()
                .filter_map(|(_, winner)| match winner.op {
                    WinnerOp::External { id, len } => Some((id, len)),
                    _ => None,
                })
                .collect();
            let values = source.load_values(&refs, &mut source_budget).await?;
            let mut staged = Staged::default();
            let mut entries = Vec::with_capacity(raw.len());
            let mut value_at = 0usize;
            for (key, winner) in raw {
                self.fmt.check_key(&key)?;
                let entry = match winner.op {
                    WinnerOp::Inline(value) => {
                        self.fmt.check_value(&value)?;
                        Entry::inline(key, winner.order_key, value)
                    }
                    WinnerOp::External { .. } => {
                        let payload = &values[value_at];
                        value_at += 1;
                        self.fmt.check_value(payload)?;
                        let object = value::encode(self.fmt.schema_id(), payload);
                        staged.value(&object);
                        Entry::external(key, winner.order_key, object.id, object.logical_len)
                    }
                    WinnerOp::Tombstone => Entry::tombstone(key, winner.order_key),
                };
                entries.push(entry);
                rows += 1;
            }
            target_root = self.apply_prepared(target_root, entries, staged).await?;
            apply_batches += 1;
        }

        Ok(MigrationReport {
            source_schema: *source.format().schema_id(),
            target_schema: *self.format().schema_id(),
            old_root: source_root,
            new_root: target_root,
            rows,
            apply_batches,
        })
    }
}
