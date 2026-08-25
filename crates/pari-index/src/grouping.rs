use std::collections::{HashMap, HashSet};

use crate::LshIndex32;

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
/// For very large deduplication jobs, prefer [`LshIndex32::duplicate_groups`],
/// which unions bucket collisions directly and does not retain candidate edges.
pub struct CandidatePairs<'a> {
    index: &'a LshIndex32,
    buckets: Vec<&'a [u32]>,
    bucket_index: usize,
    left_index: usize,
    right_index: usize,
    seen: HashSet<(u32, u32)>,
}

impl<'a> CandidatePairs<'a> {
    fn new(index: &'a LshIndex32) -> Self {
        type OrderedBucket<'b> = (usize, u64, &'b [u32]);

        let mut ordered: Vec<OrderedBucket<'a>> = Vec::new();
        for (band, table) in index.buckets.iter().enumerate() {
            for (hash, ids) in table {
                if ids.len() >= 2 {
                    ordered.push((band, *hash, ids.as_slice()));
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
    collect_default_groups(
        &mut union_find,
        keys.into_iter().enumerate().map(|(id, key)| (id, key)),
        min_size,
    )
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
        keys.into_iter().enumerate().map(|(id, key)| (id, key)),
        min_size,
        representative,
    )
}

impl LshIndex32 {
    /// Stream unique candidate pairs generated by LSH bucket collisions.
    ///
    /// Removed keys are skipped. Pair order is deterministic for a fixed index,
    /// and each returned pair is normalized to `(smaller_key, larger_key)`.
    #[must_use]
    pub fn candidate_pairs(&self) -> CandidatePairs<'_> {
        CandidatePairs::new(self)
    }

    /// Group all LSH-connected candidates, returning components of size two or larger.
    ///
    /// This path scans buckets and unions internal IDs directly. It does not
    /// construct or retain a candidate-edge list, so auxiliary memory is O(the
    /// index's internal item slots).
    #[must_use]
    pub fn duplicate_groups(&self) -> Vec<DuplicateGroup> {
        self.duplicate_groups_with(2, |_, _| true)
    }

    /// Group LSH candidates after optional application-level pair verification.
    ///
    /// The verifier receives normalized external keys before two currently
    /// disconnected components are joined. Accepted pairs are never verified
    /// again once their components are connected. Rejected pairs may be seen in
    /// more than one LSH band; this intentionally avoids an O(candidate edges)
    /// rejection cache on the large-data path.
    #[must_use]
    pub fn duplicate_groups_with<F>(&self, min_size: usize, mut verify: F) -> Vec<DuplicateGroup>
    where
        F: FnMut(u64, u64) -> bool,
    {
        let mut union_find = UnionFind::with_len(self.id_to_key.len());
        union_index_buckets(self, &mut union_find, &mut verify);
        collect_default_groups(
            &mut union_find,
            self.id_to_key
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
        let mut union_find = UnionFind::with_len(self.id_to_key.len());
        union_index_buckets(self, &mut union_find, &mut verify);
        collect_selected_groups(
            &mut union_find,
            self.id_to_key
                .iter()
                .enumerate()
                .filter_map(|(id, key)| key.map(|value| (id, value))),
            min_size,
            representative,
        )
    }
}

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

fn union_index_buckets<F>(index: &LshIndex32, union_find: &mut UnionFind, verify: &mut F)
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

fn live_key(index: &LshIndex32, id: u32) -> Option<u64> {
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
    let mut components = collect_components(union_find, members, min_size);
    let mut groups = Vec::with_capacity(components.len());
    for members in components.drain(..) {
        let representative = members[0];
        groups.push(DuplicateGroup {
            representative,
            members,
        });
    }
    groups.sort_unstable_by(|left, right| {
        left.representative
            .cmp(&right.representative)
            .then_with(|| left.members.cmp(&right.members))
    });
    groups
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

fn collect_components<I>(
    union_find: &mut UnionFind,
    members: I,
    min_size: usize,
) -> Vec<Vec<u64>>
where
    I: IntoIterator<Item = (usize, u64)>,
{
    let minimum = min_size.max(1);
    let mut by_root: HashMap<usize, Vec<u64>> = HashMap::new();
    for (id, key) in members {
        if id >= union_find.len() {
            continue;
        }
        let root = union_find.find(id);
        by_root.entry(root).or_default().push(key);
    }

    let mut components: Vec<_> = by_root
        .into_values()
        .filter_map(|mut values| {
            if values.len() < minimum {
                return None;
            }
            values.sort_unstable();
            Some(values)
        })
        .collect();
    components.sort_unstable();
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
    use std::collections::VecDeque;

    use pari_core::MinHash32;

    use super::{
        group_pairs, group_pairs_with_representative, DuplicateGroup, GroupError, LshIndex32,
    };
    use crate::LshParams;

    fn signature(seed: u64, values: impl IntoIterator<Item = u64>) -> MinHash32 {
        let mut sketch = MinHash32::new(128, seed).expect("valid sketch");
        for value in values {
            sketch.update(&value.to_le_bytes());
        }
        sketch
    }

    fn member_lists(groups: &[DuplicateGroup]) -> Vec<Vec<u64>> {
        groups.iter().map(|group| group.members().to_vec()).collect()
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
            if state & 3 == 0 {
                edges.push((left as u64, right as u64));
            }
        }

        let actual = member_lists(&group_pairs(edges.iter().copied(), 1));
        let expected = reference_components(node_count, &edges);
        assert_eq!(actual, expected);
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
