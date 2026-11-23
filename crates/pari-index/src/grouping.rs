use std::collections::{HashMap, HashSet};

use crate::{Bucket, IndexData, LshIndex32, LshIndex64};

/// One deterministic connected component of duplicate candidates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuplicateGroup {
    representative: u64,
    members: Vec<u64>,
}

impl DuplicateGroup {
    /// Return the selected representative key for this group.
    #[must_use]
    pub const fn representative(&self) -> u64 {
        self.representative
    }

    /// Borrow sorted member keys.
    #[must_use]
    pub fn members(&self) -> &[u64] {
        &self.members
    }

    /// Return the number of members in the group.
    #[must_use]
    pub fn len(&self) -> usize {
        self.members.len()
    }

    /// Return whether the group has no members.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }
}

/// Errors produced by representative-selection hooks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupError {
    /// The representative callback selected a key outside its component.
    RepresentativeNotMember { representative: u64 },
}

impl std::fmt::Display for GroupError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RepresentativeNotMember { representative } => write!(
                formatter,
                "representative {representative} is not a member of its duplicate group"
            ),
        }
    }
}

impl std::error::Error for GroupError {}

/// A streaming iterator over unique LSH candidate pairs.
///
/// The iterator does not materialize an output edge list, but uniqueness across
/// multiple LSH bands requires retaining one compact internal-ID pair per edge.
/// For very large deduplication jobs, prefer [`LshIndex32::duplicate_groups`]
/// or [`LshIndex64::duplicate_groups`], which union bucket collisions directly
/// and do not retain candidate edges.
pub struct CandidatePairs<'a> {
    index: &'a IndexData,
    buckets: Vec<&'a [u32]>,
    bucket_index: usize,
    left_index: usize,
    right_index: usize,
    seen: HashSet<(u32, u32)>,
}

impl<'a> CandidatePairs<'a> {
    fn new(index: &'a IndexData) -> Self {
        type OrderedBucket<'b> = (usize, u64, &'b [u32]);

        let mut ordered: Vec<OrderedBucket<'a>> = Vec::new();
        for (band, table) in index.buckets.iter().enumerate() {
            for (hash, ids) in table {
                if ids.len() >= 2 {
                    ordered.push((band, *hash, &**ids));
                }
            }
        }
        ordered.sort_unstable_by_key(|(band, hash, _)| (*band, *hash));
        let buckets = ordered.into_iter().map(|(_, _, ids)| ids).collect();

        Self {
            index,
            buckets,
            bucket_index: 0,
            left_index: 0,
            right_index: 1,
            seen: HashSet::new(),
        }
    }
}

impl Iterator for CandidatePairs<'_> {
    type Item = (u64, u64);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let ids = *self.buckets.get(self.bucket_index)?;
            if self.left_index + 1 >= ids.len() {
                self.bucket_index += 1;
                self.left_index = 0;
                self.right_index = 1;
                continue;
            }
            if self.right_index >= ids.len() {
                self.left_index += 1;
                self.right_index = self.left_index + 1;
                continue;
            }

            let left = ids[self.left_index];
            let right = ids[self.right_index];
            self.right_index += 1;
            if left == right {
                continue;
            }
            let id_pair = ordered_pair(left, right);
            if !self.seen.insert(id_pair) {
                continue;
            }
            let Some(left_key) = live_key(self.index, id_pair.0) else {
                continue;
            };
            let Some(right_key) = live_key(self.index, id_pair.1) else {
                continue;
            };
            return Some(ordered_pair(left_key, right_key));
        }
    }
}

/// Group an arbitrary stream of key pairs into deterministic connected components.
///
/// Edges are consumed once and are not retained. Auxiliary memory is O(unique
/// keys), consisting of the key-to-node map and union-find arrays. Self edges
/// introduce their key without changing connectivity; duplicate edges are safe.
#[must_use]
pub fn group_pairs<I>(pairs: I, min_size: usize) -> Vec<DuplicateGroup>
where
    I: IntoIterator<Item = (u64, u64)>,
{
    let (mut union_find, keys) = union_stream(pairs);
    collect_default_groups(&mut union_find, keys.into_iter().enumerate(), min_size)
}

/// Group a stream of key pairs and choose a representative from each component.
///
/// The selector receives sorted group members. Returning a key outside those
/// members is rejected instead of silently persisting an invalid representative.
pub fn group_pairs_with_representative<I, F>(
    pairs: I,
    min_size: usize,
    representative: F,
) -> Result<Vec<DuplicateGroup>, GroupError>
where
    I: IntoIterator<Item = (u64, u64)>,
    F: FnMut(&[u64]) -> u64,
{
    let (mut union_find, keys) = union_stream(pairs);
    collect_selected_groups(
        &mut union_find,
        keys.into_iter().enumerate(),
        min_size,
        representative,
    )
}

macro_rules! impl_index_grouping {
    ($index:ty) => {
        impl $index {
            /// Stream unique candidate pairs generated by LSH bucket collisions.
            ///
            /// Removed keys are skipped. Pair order is deterministic for a fixed index,
            /// and each returned pair is normalized to `(smaller_key, larger_key)`.
            #[must_use]
            pub fn candidate_pairs(&self) -> CandidatePairs<'_> {
                CandidatePairs::new(&self.data)
            }

            /// Group all LSH-connected candidates, returning components of size two or larger.
            ///
            /// This path scans buckets and unions internal IDs directly. It does not
            /// construct or retain a candidate-edge list, so auxiliary memory is O(the
            /// index's internal item slots).
            #[must_use]
            pub fn duplicate_groups(&self) -> Vec<DuplicateGroup> {
                let mut union_find = UnionFind::with_len(self.data.id_to_key.len());
                union_index_buckets_unverified(&self.data, &mut union_find);
                collect_default_groups(
                    &mut union_find,
                    self.data
                        .id_to_key
                        .iter()
                        .enumerate()
                        .filter_map(|(id, key)| key.map(|value| (id, value))),
                    2,
                )
            }

            /// Group LSH candidates after optional application-level pair verification.
            ///
            /// The verifier receives normalized external keys before two currently
            /// disconnected components are joined. Accepted pairs are never verified
            /// again once their components are connected. Rejected pairs may be seen in
            /// more than one LSH band; this intentionally avoids an O(candidate edges)
            /// rejection cache on the large-data path.
            #[must_use]
            pub fn duplicate_groups_with<F>(
                &self,
                min_size: usize,
                mut verify: F,
            ) -> Vec<DuplicateGroup>
            where
                F: FnMut(u64, u64) -> bool,
            {
                let mut union_find = UnionFind::with_len(self.data.id_to_key.len());
                union_index_buckets(&self.data, &mut union_find, &mut verify);
                collect_default_groups(
                    &mut union_find,
                    self.data
                        .id_to_key
                        .iter()
                        .enumerate()
                        .filter_map(|(id, key)| key.map(|value| (id, value))),
                    min_size,
                )
            }

            /// Group verified LSH candidates and choose a representative for each component.
            ///
            /// The representative callback receives sorted component members.
            pub fn duplicate_groups_with_representative<V, R>(
                &self,
                min_size: usize,
                mut verify: V,
                representative: R,
            ) -> Result<Vec<DuplicateGroup>, GroupError>
            where
                V: FnMut(u64, u64) -> bool,
                R: FnMut(&[u64]) -> u64,
            {
                let mut union_find = UnionFind::with_len(self.data.id_to_key.len());
                union_index_buckets(&self.data, &mut union_find, &mut verify);
                collect_selected_groups(
                    &mut union_find,
                    self.data
                        .id_to_key
                        .iter()
                        .enumerate()
                        .filter_map(|(id, key)| key.map(|value| (id, value))),
                    min_size,
                    representative,
                )
            }
        }
    };
}

impl_index_grouping!(LshIndex32);
impl_index_grouping!(LshIndex64);

fn union_stream<I>(pairs: I) -> (UnionFind, Vec<u64>)
where
    I: IntoIterator<Item = (u64, u64)>,
{
    let mut ids = HashMap::new();
    let mut keys = Vec::new();
    let mut union_find = UnionFind::default();

    for (left, right) in pairs {
        let left_id = intern_key(left, &mut ids, &mut keys, &mut union_find);
        let right_id = intern_key(right, &mut ids, &mut keys, &mut union_find);
        union_find.union(left_id, right_id);
    }
    (union_find, keys)
}

fn intern_key(
    key: u64,
    ids: &mut HashMap<u64, usize>,
    keys: &mut Vec<u64>,
    union_find: &mut UnionFind,
) -> usize {
    if let Some(&id) = ids.get(&key) {
        return id;
    }
    let id = keys.len();
    keys.push(key);
    union_find.push();
    ids.insert(key, id);
    id
}

fn union_index_buckets<F>(index: &IndexData, union_find: &mut UnionFind, verify: &mut F)
where
    F: FnMut(u64, u64) -> bool,
{
    for table in &index.buckets {
        for ids in table.values() {
            for left_index in 0..ids.len() {
                for right_index in (left_index + 1)..ids.len() {
                    let left_id = ids[left_index];
                    let right_id = ids[right_index];
                    let Ok(left_node) = usize::try_from(left_id) else {
                        continue;
                    };
                    let Ok(right_node) = usize::try_from(right_id) else {
                        continue;
                    };
                    if union_find.connected(left_node, right_node) {
                        continue;
                    }
                    let Some(left_key) = live_key(index, left_id) else {
                        continue;
                    };
                    let Some(right_key) = live_key(index, right_id) else {
                        continue;
                    };
                    let (left_key, right_key) = ordered_pair(left_key, right_key);
                    if verify(left_key, right_key) {
                        union_find.union(left_node, right_node);
                    }
                }
            }
        }
    }
}

/// Buckets across all bands below which grouping scans on the calling thread.
const PARALLEL_SCAN_MIN_BUCKETS: usize = 1 << 16;

/// Links sent from a band-scanning thread to the union thread at a time.
const LINK_BATCH: usize = 1 << 16;

fn union_index_buckets_unverified(index: &IndexData, union_find: &mut UnionFind) {
    let bucket_count: usize = index.buckets.iter().map(HashMap::len).sum();
    // Ask for the CPU count only for indexes worth scanning in parallel.
    let threads = if bucket_count < PARALLEL_SCAN_MIN_BUCKETS {
        1
    } else {
        std::thread::available_parallelism()
            .map_or(1, std::num::NonZeroUsize::get)
            .min(index.buckets.len())
    };
    if threads < 2 {
        for table in &index.buckets {
            scan_band(table, &index.id_to_key, |anchor, node| {
                union_find.union(anchor, node);
            });
        }
        return;
    }

    // Most buckets of a large index hold one key, and reading them dominates
    // grouping. Bands are scanned on separate threads, which send links only
    // for buckets with two or more live keys; this thread applies them. The
    // resulting partition does not depend on the order of the unions.
    let buckets = &index.buckets;
    let id_to_key = &index.id_to_key;
    std::thread::scope(|scope| {
        let (sender, receiver) = std::sync::mpsc::sync_channel::<Vec<(usize, usize)>>(threads * 2);
        for first_band in 0..threads {
            let sender = sender.clone();
            scope.spawn(move || {
                let mut links = Vec::with_capacity(LINK_BATCH);
                for table in buckets.iter().skip(first_band).step_by(threads) {
                    scan_band(table, id_to_key, |anchor, node| links.push((anchor, node)));
                    if links.len() >= LINK_BATCH {
                        if sender.send(std::mem::take(&mut links)).is_err() {
                            return;
                        }
                        links.reserve(LINK_BATCH);
                    }
                }
                if !links.is_empty() {
                    let _ = sender.send(links);
                }
            });
        }
        drop(sender);
        for links in receiver {
            for (anchor, node) in links {
                union_find.union(anchor, node);
            }
        }
    });
}

/// Call `link(anchor, node)` for every live key of each bucket in `table`
/// after its first live key, the bucket's anchor.
fn scan_band(
    table: &HashMap<u64, Bucket>,
    id_to_key: &[Option<u64>],
    mut link: impl FnMut(usize, usize),
) {
    for ids in table.values() {
        if ids.len() < 2 {
            continue;
        }
        let mut anchor = None;
        for &id in ids.iter() {
            let Ok(node) = usize::try_from(id) else {
                continue;
            };
            if id_to_key.get(node).copied().flatten().is_none() {
                continue;
            }
            if let Some(anchor_node) = anchor {
                link(anchor_node, node);
            } else {
                anchor = Some(node);
            }
        }
    }
}

fn live_key(index: &IndexData, id: u32) -> Option<u64> {
    let index_id = usize::try_from(id).ok()?;
    index.id_to_key.get(index_id).copied().flatten()
}

fn collect_default_groups<I>(
    union_find: &mut UnionFind,
    members: I,
    min_size: usize,
) -> Vec<DuplicateGroup>
where
    I: IntoIterator<Item = (usize, u64)>,
{
    // Components arrive ordered by their smallest key, which is each default
    // representative, so the groups need no further sort.
    collect_components(union_find, members, min_size)
        .into_iter()
        .map(|members| DuplicateGroup {
            representative: members[0],
            members,
        })
        .collect()
}

fn collect_selected_groups<I, F>(
    union_find: &mut UnionFind,
    members: I,
    min_size: usize,
    mut representative: F,
) -> Result<Vec<DuplicateGroup>, GroupError>
where
    I: IntoIterator<Item = (usize, u64)>,
    F: FnMut(&[u64]) -> u64,
{
    let components = collect_components(union_find, members, min_size);
    let mut groups = Vec::with_capacity(components.len());
    for members in components {
        let selected = representative(&members);
        if members.binary_search(&selected).is_err() {
            return Err(GroupError::RepresentativeNotMember {
                representative: selected,
            });
        }
        groups.push(DuplicateGroup {
            representative: selected,
            members,
        });
    }
    groups.sort_unstable_by(|left, right| {
        left.representative
            .cmp(&right.representative)
            .then_with(|| left.members.cmp(&right.members))
    });
    Ok(groups)
}

/// Gather member keys by component, keep components of at least `min_size`,
/// and return each sorted, ordered by smallest key.
///
/// Components are disjoint, so ordering them by smallest key is the same as
/// ordering them lexicographically. Sizes and placement use flat arrays
/// indexed by node rather than one allocation per component, so the many
/// singleton components of a large index cost no more than a counter.
fn collect_components<I>(union_find: &mut UnionFind, members: I, min_size: usize) -> Vec<Vec<u64>>
where
    I: IntoIterator<Item = (usize, u64)>,
{
    let minimum = min_size.max(1);
    let nodes = union_find.len();
    let mut entries: Vec<(usize, u64)> = Vec::new();
    let mut sizes = vec![0_usize; nodes];
    for (id, key) in members {
        if id >= nodes {
            continue;
        }
        let root = union_find.find(id);
        sizes[root] += 1;
        entries.push((root, key));
    }

    // Each kept component's slot in `flat`; `usize::MAX` marks the rest.
    let mut cursors = vec![usize::MAX; nodes];
    let mut kept = Vec::new();
    let mut total = 0_usize;
    for (root, &size) in sizes.iter().enumerate() {
        if size >= minimum {
            cursors[root] = total;
            kept.push((total, size));
            total += size;
        }
    }
    drop(sizes);

    let mut flat = vec![0_u64; total];
    for (root, key) in entries {
        let cursor = &mut cursors[root];
        if *cursor != usize::MAX {
            flat[*cursor] = key;
            *cursor += 1;
        }
    }

    let mut components: Vec<Vec<u64>> = kept
        .into_iter()
        .map(|(start, size)| {
            let mut values = flat[start..start + size].to_vec();
            values.sort_unstable();
            values
        })
        .collect();
    components.sort_unstable_by_key(|values| values[0]);
    components
}

fn ordered_pair<T: Ord + Copy>(left: T, right: T) -> (T, T) {
    if left <= right {
        (left, right)
    } else {
        (right, left)
    }
}

#[derive(Debug, Default)]
struct UnionFind {
    parent: Vec<usize>,
    rank: Vec<u8>,
}

impl UnionFind {
    fn with_len(length: usize) -> Self {
        Self {
            parent: (0..length).collect(),
            rank: vec![0; length],
        }
    }

    fn len(&self) -> usize {
        self.parent.len()
    }

    fn push(&mut self) {
        self.parent.push(self.parent.len());
        self.rank.push(0);
    }

    fn find(&mut self, node: usize) -> usize {
        let mut root = node;
        while self.parent[root] != root {
            root = self.parent[root];
        }

        let mut current = node;
        while self.parent[current] != current {
            let next = self.parent[current];
            self.parent[current] = root;
            current = next;
        }
        root
    }

    fn connected(&mut self, left: usize, right: usize) -> bool {
        self.find(left) == self.find(right)
    }

    fn union(&mut self, left: usize, right: usize) {
        let mut left_root = self.find(left);
        let mut right_root = self.find(right);
        if left_root == right_root {
            return;
        }
        if self.rank[left_root] < self.rank[right_root] {
            std::mem::swap(&mut left_root, &mut right_root);
        }
        self.parent[right_root] = left_root;
        if self.rank[left_root] == self.rank[right_root] {
            self.rank[left_root] = self.rank[left_root].saturating_add(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, VecDeque};

    use pari_core::{MinHash32, MinHash64};

    use super::{
        collect_components, group_pairs, group_pairs_with_representative, DuplicateGroup,
        GroupError, LshIndex32, LshIndex64, UnionFind,
    };
    use crate::LshParams;

    fn signature(seed: u64, values: impl IntoIterator<Item = u64>) -> MinHash32 {
        let mut sketch = MinHash32::new(128, seed).expect("valid sketch");
        for value in values {
            sketch.update(&value.to_le_bytes());
        }
        sketch
    }

    fn signature64(seed: u64, values: impl IntoIterator<Item = u64>) -> MinHash64 {
        let mut sketch = MinHash64::new(128, seed).expect("valid sketch");
        for value in values {
            sketch.update(&value.to_le_bytes());
        }
        sketch
    }

    fn member_lists(groups: &[DuplicateGroup]) -> Vec<Vec<u64>> {
        groups
            .iter()
            .map(|group| group.members().to_vec())
            .collect()
    }

    #[test]
    fn pair_grouping_handles_self_and_duplicate_edges() {
        let groups = group_pairs([(3, 3), (1, 2), (2, 1), (2, 3), (8, 8)], 1);
        assert_eq!(member_lists(&groups), vec![vec![1, 2, 3], vec![8]]);
        assert_eq!(groups[0].representative(), 1);
    }

    #[test]
    fn representative_hook_must_choose_a_member() {
        let groups = group_pairs_with_representative([(1, 2), (2, 3)], 2, |members| {
            members[members.len() - 1]
        })
        .expect("valid representative");
        assert_eq!(groups[0].representative(), 3);

        assert_eq!(
            group_pairs_with_representative([(1, 2)], 2, |_| 99),
            Err(GroupError::RepresentativeNotMember { representative: 99 })
        );
    }

    #[test]
    fn candidate_pair_iterator_is_unique_across_bands() {
        let same = signature(4, 0..100);
        let mut index =
            LshIndex32::with_params(0.8, 128, 4, LshParams::new(32, 4)).expect("valid index");
        index
            .insert_many([(9, &same), (1, &same), (5, &same)])
            .expect("valid insert");

        let mut pairs: Vec<_> = index.candidate_pairs().collect();
        pairs.sort_unstable();
        assert_eq!(pairs, vec![(1, 5), (1, 9), (5, 9)]);
    }

    #[test]
    fn parallel_bucket_scan_matches_pair_grouping() {
        // Enough buckets to take the parallel scan: 32 bands of 4 rows over
        // 6,000 keys. Keys come in families sharing most features, so groups
        // span several bands, and some keys are removed.
        let mut index =
            LshIndex32::with_params(0.5, 128, 6, LshParams::new(32, 4)).expect("valid index");
        let sketches: Vec<_> = (0..6_000_u64)
            .map(|key| {
                let family = key / 3;
                let base = family * 1_000;
                signature(6, (base..base + 20).chain([base + 500 + key % 3]))
            })
            .collect();
        index
            .insert_many((0..6_000_u64).zip(&sketches))
            .expect("valid insert");
        for key in (0..6_000_u64).step_by(7) {
            assert!(index.remove(key));
        }
        let buckets: usize = index.data.buckets.iter().map(HashMap::len).sum();
        assert!(
            buckets >= super::PARALLEL_SCAN_MIN_BUCKETS,
            "{buckets} buckets"
        );

        let groups = index.duplicate_groups();
        assert!(groups.len() > 500, "{} groups", groups.len());
        assert_eq!(groups, group_pairs(index.candidate_pairs(), 2));
    }

    #[test]
    fn direct_grouping_uses_verification_and_skips_removed_keys() {
        let same = signature(8, 0..100);
        let distant = signature(8, 1_000..1_100);
        let mut index =
            LshIndex32::with_params(0.8, 128, 8, LshParams::new(32, 4)).expect("valid index");
        index
            .insert_many([(1, &same), (2, &same), (3, &same), (99, &distant)])
            .expect("valid insert");
        assert!(index.remove(3));

        let groups = index.duplicate_groups_with(1, |left, right| (left, right) == (1, 2));
        assert_eq!(member_lists(&groups), vec![vec![1, 2], vec![99]]);
    }

    #[test]
    fn direct_grouping_defaults_to_components_of_two_or_more() {
        let same = signature(3, 0..100);
        let distant = signature(3, 10_000..10_100);
        let mut index =
            LshIndex32::with_params(0.8, 128, 3, LshParams::new(32, 4)).expect("valid index");
        index
            .insert_many([(10, &same), (11, &same), (50, &distant)])
            .expect("valid insert");

        assert_eq!(member_lists(&index.duplicate_groups()), vec![vec![10, 11]]);
    }

    #[test]
    fn direct_grouping_selects_a_live_anchor_after_removal() {
        let same = signature(6, 0..100);
        let mut index =
            LshIndex32::with_params(0.8, 128, 6, LshParams::new(32, 4)).expect("valid index");
        index
            .insert_many([(1, &same), (2, &same), (3, &same)])
            .expect("valid batch");
        assert!(index.remove(1));

        assert_eq!(member_lists(&index.duplicate_groups()), vec![vec![2, 3]]);
    }

    #[test]
    fn index64_candidate_pairs_and_grouping_match_the_index_contract() {
        let same = signature64(14, 0..100);
        let far = signature64(14, 1_000..1_100);
        let mut index =
            LshIndex64::with_params(0.8, 128, 14, LshParams::new(32, 4)).expect("index");
        index
            .insert_many([(9, &same), (1, &same), (5, &same), (99, &far)])
            .expect("insert");

        let mut pairs: Vec<_> = index.candidate_pairs().collect();
        pairs.sort_unstable();
        assert_eq!(pairs, vec![(1, 5), (1, 9), (5, 9)]);
        assert_eq!(member_lists(&index.duplicate_groups()), vec![vec![1, 5, 9]]);
        assert_eq!(
            member_lists(
                &index.duplicate_groups_with(1, |left, right| { (left, right) == (1, 5) })
            ),
            vec![vec![1, 5], vec![9], vec![99]]
        );
        let selected = index
            .duplicate_groups_with_representative(
                2,
                |_, _| true,
                |members| members[members.len() - 1],
            )
            .expect("representative");
        assert_eq!(selected[0].representative(), 9);
        assert_eq!(selected[0].members(), &[1, 5, 9]);

        assert!(index.remove(1));
        assert_eq!(member_lists(&index.duplicate_groups()), vec![vec![5, 9]]);
    }

    #[test]
    fn randomized_stream_matches_reference_graph_components() {
        let node_count = 64_usize;
        let mut state = 0xD1B5_4A32_D192_ED03_u64;
        let mut edges: Vec<(u64, u64)> = (0..node_count)
            .map(|node| {
                let node = u64::try_from(node).expect("small test node");
                (node, node)
            })
            .collect();

        for _ in 0..256 {
            state = splitmix(state);
            let left = usize::try_from(state % node_count as u64).expect("small test node");
            state = splitmix(state);
            let right = usize::try_from(state % node_count as u64).expect("small test node");
            edges.push((left as u64, right as u64));
            if state.trailing_zeros() >= 2 {
                edges.push((left as u64, right as u64));
            }
        }

        let actual = member_lists(&group_pairs(edges.iter().copied(), 1));
        let expected = reference_components(node_count, &edges);
        assert_eq!(actual, expected);
    }

    /// The previous implementation: one vector per root in a hash map.
    fn hash_map_components(
        union_find: &mut UnionFind,
        members: &[(usize, u64)],
        min_size: usize,
    ) -> Vec<Vec<u64>> {
        let minimum = min_size.max(1);
        let mut by_root: HashMap<usize, Vec<u64>> = HashMap::new();
        for &(id, key) in members {
            if id >= union_find.len() {
                continue;
            }
            let root = union_find.find(id);
            by_root.entry(root).or_default().push(key);
        }
        let mut components: Vec<_> = by_root
            .into_values()
            .filter(|values| values.len() >= minimum)
            .map(|mut values| {
                values.sort_unstable();
                values
            })
            .collect();
        components.sort_unstable();
        components
    }

    #[test]
    fn flat_component_collection_matches_the_hash_map_reference() {
        let mut state = 0x5EED_0FC0_11EC_7500_u64;
        for round in 0..200_u64 {
            state = splitmix(state ^ round);
            let nodes = 1 + usize::try_from(state % 300).expect("small node count");
            let mut union_find = UnionFind::with_len(nodes);
            state = splitmix(state);
            let unions = usize::try_from(state % 400).expect("small union count");
            for _ in 0..unions {
                state = splitmix(state);
                let left = usize::try_from(state % nodes as u64).expect("node");
                state = splitmix(state);
                let right = usize::try_from(state % nodes as u64).expect("node");
                union_find.union(left, right);
            }
            // Members skip some nodes, include out-of-range ids, and map ids
            // to keys that are not in id order.
            let mut members = Vec::new();
            for id in 0..nodes + 5 {
                state = splitmix(state);
                if state % 7 != 0 {
                    members.push((id, splitmix(id as u64 ^ round) % 10_000));
                }
            }
            members.sort_unstable_by_key(|&(_, key)| key);
            members.dedup_by_key(|&mut (_, key)| key);
            for min_size in 0..4 {
                let mut reference = UnionFind {
                    parent: union_find.parent.clone(),
                    rank: union_find.rank.clone(),
                };
                assert_eq!(
                    collect_components(&mut union_find, members.iter().copied(), min_size),
                    hash_map_components(&mut reference, &members, min_size),
                    "round {round} min_size {min_size}"
                );
            }
        }
    }

    fn reference_components(node_count: usize, edges: &[(u64, u64)]) -> Vec<Vec<u64>> {
        let mut adjacency = vec![Vec::new(); node_count];
        for &(left, right) in edges {
            let left = usize::try_from(left).expect("test key fits usize");
            let right = usize::try_from(right).expect("test key fits usize");
            if left != right {
                adjacency[left].push(right);
                adjacency[right].push(left);
            }
        }

        let mut visited = vec![false; node_count];
        let mut groups = Vec::new();
        for start in 0..node_count {
            if visited[start] {
                continue;
            }
            let mut queue = VecDeque::from([start]);
            visited[start] = true;
            let mut group = Vec::new();
            while let Some(node) = queue.pop_front() {
                group.push(u64::try_from(node).expect("small test node"));
                for &next in &adjacency[node] {
                    if !visited[next] {
                        visited[next] = true;
                        queue.push_back(next);
                    }
                }
            }
            group.sort_unstable();
            groups.push(group);
        }
        groups.sort_unstable();
        groups
    }

    fn splitmix(mut value: u64) -> u64 {
        value = value.wrapping_add(0x9E37_79B9_7F4A_7C15);
        value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        value ^ (value >> 31)
    }
}
