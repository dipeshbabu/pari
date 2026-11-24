//! Duplicate grouping in bounded memory for inputs too large for an in-memory
//! LSH index.
//!
//! Each record contributes one `(band, hash, ordinal)` entry per LSH band.
//! Entries are sorted in memory-bounded runs and spilled to disk, then merged
//! so that each bucket's members arrive together. Members of a bucket are
//! unioned, after the optional similarity check, exactly as the in-memory
//! index groups them; components do not depend on the order buckets are
//! visited, so the groups are identical. Memory holds the keys and a
//! union-find over record ordinals (about 16 bytes per record), one sort
//! buffer, and the largest bucket. Signatures for `--min-similarity` stay in
//! a fixed-width file and are read only for buckets with several members.

use std::{
    cmp::Reverse,
    collections::BinaryHeap,
    error::Error,
    fs::{self, File, OpenOptions},
    io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use pari_index::LshError;

const ENTRY_BYTES: usize = 16;
const READ_BUFFER_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Entry {
    band: u32,
    hash: u64,
    ordinal: u32,
}

impl Entry {
    fn encode(self) -> [u8; ENTRY_BYTES] {
        let mut raw = [0_u8; ENTRY_BYTES];
        raw[..4].copy_from_slice(&self.band.to_le_bytes());
        raw[4..12].copy_from_slice(&self.hash.to_le_bytes());
        raw[12..].copy_from_slice(&self.ordinal.to_le_bytes());
        raw
    }

    fn decode(raw: &[u8; ENTRY_BYTES]) -> Self {
        let mut band = [0_u8; 4];
        let mut hash = [0_u8; 8];
        let mut ordinal = [0_u8; 4];
        band.copy_from_slice(&raw[..4]);
        hash.copy_from_slice(&raw[4..12]);
        ordinal.copy_from_slice(&raw[12..]);
        Self {
            band: u32::from_le_bytes(band),
            hash: u64::from_le_bytes(hash),
            ordinal: u32::from_le_bytes(ordinal),
        }
    }
}

/// Temporary files removed when the run ends, successfully or not.
struct Scratch {
    directory: PathBuf,
    prefix: String,
    paths: Vec<PathBuf>,
}

impl Scratch {
    fn new(directory: &Path) -> Result<Self, Box<dyn Error>> {
        fs::create_dir_all(directory)?;
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| "system clock is before the UNIX epoch")?
            .as_nanos();
        Ok(Self {
            directory: directory.to_path_buf(),
            prefix: format!(".pari-dedup-{}-{nonce:x}", std::process::id()),
            paths: Vec::new(),
        })
    }

    fn create(&mut self, name: &str) -> Result<(PathBuf, File), Box<dyn Error>> {
        let path = self.directory.join(format!("{}-{name}.tmp", self.prefix));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)?;
        self.paths.push(path.clone());
        Ok((path, file))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        for path in &self.paths {
            let _ = fs::remove_file(path);
        }
    }
}

/// Collects records, then groups them by LSH collisions in bounded memory.
pub struct BoundedDedup {
    scratch: Scratch,
    max_buffer: usize,
    buffer: Vec<Entry>,
    runs: Vec<PathBuf>,
    keys: Vec<u64>,
    signatures: Option<(BufWriter<File>, PathBuf, usize)>,
    value_bytes: usize,
    minimum: Option<f64>,
}

impl BoundedDedup {
    /// Signatures are `num_perm` little-endian values of `value_bytes` each;
    /// they are stored only when `minimum` is set.
    pub fn new(
        spill_dir: &Path,
        max_buffer: usize,
        minimum: Option<f64>,
        num_perm: usize,
        value_bytes: usize,
    ) -> Result<Self, Box<dyn Error>> {
        let signature_bytes = num_perm
            .checked_mul(value_bytes)
            .ok_or("signature width overflows")?;
        if max_buffer == 0 {
            return Err("--max-buffer-records must be positive".into());
        }
        let mut scratch = Scratch::new(spill_dir)?;
        let signatures = if minimum.is_some() {
            let (path, file) = scratch.create("signatures")?;
            Some((BufWriter::new(file), path, signature_bytes))
        } else {
            None
        };
        Ok(Self {
            scratch,
            max_buffer,
            buffer: Vec::new(),
            runs: Vec::new(),
            keys: Vec::new(),
            signatures,
            value_bytes,
            minimum,
        })
    }

    /// Add one record with its band hashes and little-endian signature.
    pub fn push(
        &mut self,
        key: u64,
        band_hashes: &[u64],
        signature: &[u8],
    ) -> Result<(), Box<dyn Error>> {
        let ordinal = u32::try_from(self.keys.len())
            .map_err(|_| "bounded dedup supports at most 4,294,967,295 records")?;
        self.keys.push(key);
        if let Some((writer, _, width)) = &mut self.signatures {
            if signature.len() != *width {
                return Err("signature width does not match the scheme".into());
            }
            writer.write_all(signature)?;
        }
        for (band, hash) in (0_u32..).zip(band_hashes.iter().copied()) {
            self.buffer.push(Entry {
                band,
                hash,
                ordinal,
            });
            if self.buffer.len() >= self.max_buffer {
                self.spill()?;
            }
        }
        Ok(())
    }

    fn spill(&mut self) -> Result<(), Box<dyn Error>> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        self.buffer.sort_unstable();
        let (path, file) = self.scratch.create(&format!("run-{}", self.runs.len()))?;
        let mut writer = BufWriter::new(file);
        for entry in &self.buffer {
            writer.write_all(&entry.encode())?;
        }
        writer.flush()?;
        self.buffer.clear();
        self.runs.push(path);
        Ok(())
    }

    /// Group every record, returning the sorted members of each group of at
    /// least `min_size`, ordered by smallest key, as the in-memory index
    /// does. The smallest key is each group's representative.
    pub fn finish(mut self, min_size: usize) -> Result<Vec<Vec<u64>>, Box<dyn Error>> {
        check_duplicate_keys(&self.keys)?;
        let mut signatures = match self.signatures.take() {
            Some((mut writer, path, width)) => {
                writer.flush()?;
                drop(writer);
                Some((File::open(path)?, width))
            }
            None => None,
        };
        // Spill the last buffer only if earlier runs exist; small inputs are
        // grouped straight from memory.
        if self.runs.is_empty() {
            self.buffer.sort_unstable();
        } else {
            self.spill()?;
        }
        let buffer = std::mem::take(&mut self.buffer);
        let runs = std::mem::take(&mut self.runs);
        let mut union_find = UnionFind::new(self.keys.len());
        let minimum = self.minimum;
        let value_bytes = self.value_bytes;
        let keys = &self.keys;
        let mut bucket = Vec::new();
        let mut rows: Vec<u8> = Vec::new();
        let mut loaded: Vec<bool> = Vec::new();
        let mut visit = |members: &[u32]| -> Result<(), Box<dyn Error>> {
            if members.len() < 2 {
                return Ok(());
            }
            match (minimum, &mut signatures) {
                (Some(minimum), Some((file, width))) => {
                    // Most buckets repeat a component an earlier band already
                    // joined; skip them without reading any signature.
                    let root = union_find.find(members[0]);
                    if members[1..]
                        .iter()
                        .all(|node| union_find.find(*node) == root)
                    {
                        return Ok(());
                    }
                    rows.resize(members.len() * *width, 0);
                    loaded.clear();
                    loaded.resize(members.len(), false);
                    for left in 0..members.len() {
                        for right in left + 1..members.len() {
                            let (left_node, right_node) = (members[left], members[right]);
                            if union_find.connected(left_node, right_node) {
                                continue;
                            }
                            for slot in [left, right] {
                                if !loaded[slot] {
                                    read_row(
                                        file,
                                        *width,
                                        members[slot],
                                        &mut rows[slot * *width..(slot + 1) * *width],
                                    )?;
                                    loaded[slot] = true;
                                }
                            }
                            let similarity = row_similarity(
                                &rows[left * *width..(left + 1) * *width],
                                &rows[right * *width..(right + 1) * *width],
                                value_bytes,
                            );
                            if similarity >= minimum {
                                union_find.union(left_node, right_node);
                            }
                        }
                    }
                }
                _ => {
                    for node in &members[1..] {
                        union_find.union(members[0], *node);
                    }
                }
            }
            Ok(())
        };

        if runs.is_empty() {
            for_each_bucket(buffer.iter().copied().map(Ok), &mut bucket, &mut visit)?;
        } else {
            for_each_bucket(MergedRuns::open(&runs)?, &mut bucket, &mut visit)?;
        }
        Ok(collect_groups(&mut union_find, keys, min_size))
    }
}

/// Fail like the in-memory index: on the key whose repeat appears first.
fn check_duplicate_keys(keys: &[u64]) -> Result<(), Box<dyn Error>> {
    let mut order: Vec<(u64, u32)> = keys.iter().copied().zip(0_u32..).collect();
    order.sort_unstable();
    let repeat = order
        .windows(2)
        .filter(|pair| pair[0].0 == pair[1].0)
        .map(|pair| (pair[1].1, pair[1].0))
        .min();
    match repeat {
        Some((_, key)) => Err(Box::new(LshError::DuplicateKey { key })),
        None => Ok(()),
    }
}

fn read_row(
    file: &mut File,
    width: usize,
    ordinal: u32,
    row: &mut [u8],
) -> Result<(), Box<dyn Error>> {
    file.seek(SeekFrom::Start(u64::from(ordinal) * u64::try_from(width)?))?;
    file.read_exact(row)?;
    Ok(())
}

/// The fraction of signature values that match, as the in-memory check
/// computes it. Values are compared at their encoded width.
fn row_similarity(left: &[u8], right: &[u8], value_bytes: usize) -> f64 {
    let values = left.len() / value_bytes;
    if values == 0 {
        return 0.0;
    }
    let matches = left
        .chunks_exact(value_bytes)
        .zip(right.chunks_exact(value_bytes))
        .filter(|(a, b)| a == b)
        .count();
    #[allow(clippy::cast_precision_loss)]
    {
        matches as f64 / values as f64
    }
}

/// Call `visit` with the ordinals of each bucket, ascending.
fn for_each_bucket(
    entries: impl Iterator<Item = Result<Entry, Box<dyn Error>>>,
    bucket: &mut Vec<u32>,
    visit: &mut impl FnMut(&[u32]) -> Result<(), Box<dyn Error>>,
) -> Result<(), Box<dyn Error>> {
    let mut current: Option<(u32, u64)> = None;
    bucket.clear();
    for entry in entries {
        let entry = entry?;
        let key = (entry.band, entry.hash);
        if current != Some(key) {
            visit(bucket)?;
            bucket.clear();
            current = Some(key);
        }
        bucket.push(entry.ordinal);
    }
    visit(bucket)
}

/// A k-way merge of sorted run files.
struct MergedRuns {
    readers: Vec<BufReader<File>>,
    heap: BinaryHeap<Reverse<(Entry, usize)>>,
}

impl MergedRuns {
    fn open(paths: &[PathBuf]) -> Result<Self, Box<dyn Error>> {
        let mut readers = Vec::with_capacity(paths.len());
        let mut heap = BinaryHeap::with_capacity(paths.len());
        for (run, path) in paths.iter().enumerate() {
            let mut reader = BufReader::with_capacity(READ_BUFFER_BYTES, File::open(path)?);
            if let Some(entry) = read_entry(&mut reader)? {
                heap.push(Reverse((entry, run)));
            }
            readers.push(reader);
        }
        Ok(Self { readers, heap })
    }
}

impl Iterator for MergedRuns {
    type Item = Result<Entry, Box<dyn Error>>;

    fn next(&mut self) -> Option<Self::Item> {
        let Reverse((entry, run)) = self.heap.pop()?;
        match read_entry(&mut self.readers[run]) {
            Ok(Some(next)) => self.heap.push(Reverse((next, run))),
            Ok(None) => {}
            Err(error) => return Some(Err(error)),
        }
        Some(Ok(entry))
    }
}

fn read_entry(reader: &mut impl Read) -> Result<Option<Entry>, Box<dyn Error>> {
    let mut raw = [0_u8; ENTRY_BYTES];
    let mut read = 0;
    while read < ENTRY_BYTES {
        match reader.read(&mut raw[read..])? {
            0 if read == 0 => return Ok(None),
            0 => return Err("truncated dedup spill run".into()),
            count => read += count,
        }
    }
    Ok(Some(Entry::decode(&raw)))
}

/// Union-find over record ordinals with union by size and path halving.
struct UnionFind {
    parent: Vec<u32>,
    size: Vec<u32>,
}

impl UnionFind {
    fn new(len: usize) -> Self {
        Self {
            parent: (0..u32::try_from(len).unwrap_or(u32::MAX)).collect(),
            size: vec![1; len],
        }
    }

    fn find(&mut self, mut node: u32) -> u32 {
        while self.parent[node as usize] != node {
            let grandparent = self.parent[self.parent[node as usize] as usize];
            self.parent[node as usize] = grandparent;
            node = grandparent;
        }
        node
    }

    fn connected(&mut self, left: u32, right: u32) -> bool {
        self.find(left) == self.find(right)
    }

    fn union(&mut self, left: u32, right: u32) {
        let (mut left, mut right) = (self.find(left), self.find(right));
        if left == right {
            return;
        }
        if self.size[left as usize] < self.size[right as usize] {
            std::mem::swap(&mut left, &mut right);
        }
        self.parent[right as usize] = left;
        self.size[left as usize] += self.size[right as usize];
    }
}

/// Components of at least `min_size` keys, each sorted, ordered by their
/// smallest key, which is the representative.
fn collect_groups(union_find: &mut UnionFind, keys: &[u64], min_size: usize) -> Vec<Vec<u64>> {
    let mut members: Vec<(u32, u64)> = Vec::new();
    for (ordinal, key) in (0_u32..).zip(keys.iter().copied()) {
        let root = union_find.find(ordinal);
        if union_find.size[root as usize] as usize >= min_size {
            members.push((root, key));
        }
    }
    members.sort_unstable();
    let mut groups: Vec<Vec<u64>> = Vec::new();
    let mut current = None;
    for (root, key) in members {
        if current != Some(root) {
            groups.push(Vec::new());
            current = Some(root);
        }
        if let Some(group) = groups.last_mut() {
            group.push(key);
        }
    }
    groups.sort_unstable_by_key(|group| group[0]);
    groups
}
