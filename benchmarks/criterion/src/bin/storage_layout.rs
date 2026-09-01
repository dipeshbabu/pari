#![allow(clippy::too_many_lines)]

use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    error::Error,
    fmt,
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use memmap2::{Mmap, MmapOptions};
use pari_core::MinHash32;
use pari_index::{LshIndex32, LshParams};
use redb::{Database, TableDefinition};
use serde::Serialize;

const NUM_PERM: usize = 128;
const SEED: u64 = 7;
const THRESHOLD: f64 = 0.8;
const BANDS: usize = 32;
const ROWS: usize = 4;
const FEATURES_PER_ITEM: u64 = 64;
const PAGE_TARGET_BYTES: usize = 32 * 1024;
const PAGE_MAGIC: [u8; 8] = *b"PARIPG1\0";
const PAGE_VERSION: u32 = 1;
const PAGE_HEADER_BYTES: usize = 16;
const PAGE_DIRECTORY_BYTES: usize = 40;
const BUCKET_RECORD_HEADER_BYTES: usize = 16;
const FNV_OFFSET_BASIS: u64 = 0xCBF2_9CE4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01B3;
const REDB_BUCKETS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("buckets");

type BenchResult<T> = Result<T, Box<dyn Error>>;

#[derive(Debug, Clone)]
struct Config {
    items: usize,
    queries: usize,
    output: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct BucketKey {
    band: u32,
    hash: u64,
}

impl BucketKey {
    fn encode(self) -> [u8; 12] {
        let mut bytes = [0_u8; 12];
        bytes[..4].copy_from_slice(&self.band.to_le_bytes());
        bytes[4..].copy_from_slice(&self.hash.to_le_bytes());
        bytes
    }
}

#[derive(Debug)]
struct Workload {
    buckets: BTreeMap<BucketKey, Vec<u64>>,
    query_buckets: Vec<Vec<BucketKey>>,
    expected: Vec<Vec<u64>>,
}

#[derive(Debug, Clone, Copy)]
struct PageDescriptor {
    first: BucketKey,
    last: BucketKey,
    offset: u64,
    length: u32,
}

#[derive(Debug, Serialize)]
struct CandidateReport {
    name: String,
    build_ms: f64,
    build_items_per_second: f64,
    reopen_ms: f64,
    file_bytes: u64,
    bytes_per_item: f64,
    scalar_p50_ms: f64,
    scalar_p95_ms: f64,
    scalar_p99_ms: f64,
    batch_queries_per_second: f64,
    rss_after_reopen_bytes: Option<u64>,
}

#[derive(Debug, Serialize)]
struct ShootoutReport {
    schema_version: u32,
    items: usize,
    queries: usize,
    num_perm: usize,
    bands: usize,
    rows: usize,
    query_mode: &'static str,
    candidates: Vec<CandidateReport>,
}

trait QueryStore {
    fn query(&mut self, buckets: &[BucketKey]) -> BenchResult<Vec<u64>>;
}

#[derive(Debug)]
struct PageStore {
    file: File,
    pages: Vec<PageDescriptor>,
}

impl PageStore {
    fn open(path: &Path) -> BenchResult<Self> {
        let mut file = File::open(path)?;
        let pages = read_page_directory(&mut file)?;
        Ok(Self { file, pages })
    }
}

impl QueryStore for PageStore {
    fn query(&mut self, buckets: &[BucketKey]) -> BenchResult<Vec<u64>> {
        let mut candidates = BTreeSet::new();
        for bucket in buckets {
            if let Some(page) = find_page(&self.pages, *bucket) {
                let bytes = read_page(&mut self.file, page)?;
                collect_bucket(&bytes, *bucket, &mut candidates)?;
            }
        }
        Ok(candidates.into_iter().collect())
    }
}

struct MmapStore {
    mmap: Mmap,
    pages: Vec<PageDescriptor>,
}

impl MmapStore {
    fn open(path: &Path) -> BenchResult<Self> {
        let file = File::open(path)?;
        // SAFETY: this benchmark owns the file and never modifies it after the
        // mapping is created. The mapping is read-only and exists only for the
        // duration of this isolated benchmark process.
        let mmap = unsafe { MmapOptions::new().map(&file)? };
        let pages = parse_page_directory(&mmap, u64::try_from(mmap.len())?)?;
        Ok(Self { mmap, pages })
    }
}

impl QueryStore for MmapStore {
    fn query(&mut self, buckets: &[BucketKey]) -> BenchResult<Vec<u64>> {
        let mut candidates = BTreeSet::new();
        for bucket in buckets {
            if let Some(page) = find_page(&self.pages, *bucket) {
                let start = usize::try_from(page.offset)?;
                let end = start
                    .checked_add(usize::try_from(page.length)?)
                    .ok_or_else(|| invalid_data("page range overflow"))?;
                let bytes = self
                    .mmap
                    .get(start..end)
                    .ok_or_else(|| invalid_data("mmap page range is out of bounds"))?;
                collect_bucket(bytes, *bucket, &mut candidates)?;
            }
        }
        Ok(candidates.into_iter().collect())
    }
}

struct RedbStore {
    database: Database,
}

impl RedbStore {
    fn open(path: &Path) -> BenchResult<Self> {
        Ok(Self {
            database: Database::open(path)?,
        })
    }
}

impl QueryStore for RedbStore {
    fn query(&mut self, buckets: &[BucketKey]) -> BenchResult<Vec<u64>> {
        let transaction = self.database.begin_read()?;
        let table = transaction.open_table(REDB_BUCKETS)?;
        let mut candidates = BTreeSet::new();
        for bucket in buckets {
            let encoded = bucket.encode();
            if let Some(value) = table.get(encoded.as_slice())? {
                collect_members(value.value(), &mut candidates)?;
            }
        }
        Ok(candidates.into_iter().collect())
    }
}

fn main() -> BenchResult<()> {
    let config = parse_config()?;
    validate_config(&config)?;
    let workload = build_workload(config.items, config.queries)?;
    let root = temporary_root()?;
    fs::create_dir_all(&root)?;

    let result = run_shootout(&root, &config, &workload);
    let cleanup_result = fs::remove_dir_all(&root);
    if let Err(error) = cleanup_result {
        eprintln!("warning: failed to remove {}: {error}", root.display());
    }
    let report = result?;
    let json = serde_json::to_string_pretty(&report)?;
    fs::write(&config.output, format!("{json}\n"))?;
    println!("{json}");
    Ok(())
}

fn parse_config() -> BenchResult<Config> {
    let mut config = Config {
        items: 5_000,
        queries: 200,
        output: PathBuf::from("storage-layout-results.json"),
    };
    let arguments: Vec<String> = env::args().skip(1).collect();
    let mut index = 0;
    while index < arguments.len() {
        let flag = &arguments[index];
        index += 1;
        match flag.as_str() {
            "--items" => config.items = parse_value(&arguments, &mut index, flag)?,
            "--queries" => config.queries = parse_value(&arguments, &mut index, flag)?,
            "--output" => config.output = PathBuf::from(next_value(&arguments, &mut index, flag)?),
            "--help" | "-h" => {
                println!("Usage: storage-layout [--items N] [--queries N] [--output PATH]");
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {other:?}").into()),
        }
    }
    Ok(config)
}

fn parse_value<T>(arguments: &[String], index: &mut usize, flag: &str) -> BenchResult<T>
where
    T: std::str::FromStr,
    T::Err: fmt::Display,
{
    let value = next_value(arguments, index, flag)?;
    value
        .parse::<T>()
        .map_err(|error| format!("invalid value for {flag}: {value:?}: {error}").into())
}

fn next_value(arguments: &[String], index: &mut usize, flag: &str) -> BenchResult<String> {
    let value = arguments
        .get(*index)
        .ok_or_else(|| format!("missing value after {flag}"))?
        .clone();
    *index += 1;
    Ok(value)
}

fn validate_config(config: &Config) -> BenchResult<()> {
    if config.items == 0 || config.queries == 0 {
        return Err("items and queries must be positive".into());
    }
    if config.queries > config.items {
        return Err("queries cannot exceed items".into());
    }
    if u32::try_from(config.items).is_err() {
        return Err("items must fit in u32".into());
    }
    if u32::try_from(config.queries).is_err() {
        return Err("queries must fit in u32".into());
    }
    Ok(())
}

fn run_shootout(root: &Path, config: &Config, workload: &Workload) -> BenchResult<ShootoutReport> {
    let page_path = root.join("paged.idx");
    let mmap_path = root.join("mmap.idx");
    let redb_path = root.join("embedded.redb");

    let page = benchmark_page(&page_path, config.items, workload)?;
    let mmap = benchmark_mmap(&mmap_path, config.items, workload)?;
    let redb = benchmark_redb(&redb_path, config.items, workload)?;

    Ok(ShootoutReport {
        schema_version: 1,
        items: config.items,
        queries: config.queries,
        num_perm: NUM_PERM,
        bands: BANDS,
        rows: ROWS,
        query_mode: "warm_after_correctness_parity_check",
        candidates: vec![page, mmap, redb],
    })
}

fn build_workload(items: usize, queries: usize) -> BenchResult<Workload> {
    let params = LshParams::new(BANDS, ROWS);
    let mut reference = LshIndex32::with_params(THRESHOLD, NUM_PERM, SEED, params)?;
    let mut signatures = Vec::with_capacity(items);
    let mut buckets: BTreeMap<BucketKey, Vec<u64>> = BTreeMap::new();

    for item in 0..items {
        let key = u64::try_from(item)?;
        let sketch = sketch_for_item(key)?;
        for bucket in buckets_for_sketch(&sketch)? {
            buckets.entry(bucket).or_default().push(key);
        }
        reference.insert(key, &sketch)?;
        signatures.push(sketch);
    }

    let mut query_buckets = Vec::with_capacity(queries);
    let mut expected = Vec::with_capacity(queries);
    for sketch in signatures.iter().take(queries) {
        query_buckets.push(buckets_for_sketch(sketch)?);
        expected.push(reference.query(sketch)?);
    }

    Ok(Workload {
        buckets,
        query_buckets,
        expected,
    })
}

fn sketch_for_item(item: u64) -> BenchResult<MinHash32> {
    let mut sketch = MinHash32::new(NUM_PERM, SEED)?;
    let base = item.wrapping_mul(100_000);
    for offset in 0..FEATURES_PER_ITEM {
        sketch.update(&base.wrapping_add(offset).to_le_bytes());
    }
    Ok(sketch)
}

fn buckets_for_sketch(sketch: &MinHash32) -> BenchResult<Vec<BucketKey>> {
    let used = BANDS
        .checked_mul(ROWS)
        .ok_or_else(|| invalid_data("band layout overflow"))?;
    let signature = sketch.signature();
    if signature.len() < used {
        return Err(invalid_data("signature shorter than benchmark band layout").into());
    }
    let mut output = Vec::with_capacity(BANDS);
    for (band, values) in signature[..used].chunks_exact(ROWS).enumerate() {
        output.push(BucketKey {
            band: u32::try_from(band)?,
            hash: hash_band(values),
        });
    }
    Ok(output)
}

fn hash_band(values: &[u32]) -> u64 {
    let mut hash = FNV_OFFSET_BASIS;
    for value in values {
        hash ^= u64::from(*value);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash ^= u64::try_from(values.len()).unwrap_or(u64::MAX);
    avalanche64(hash)
}

fn avalanche64(mut value: u64) -> u64 {
    value ^= value >> 33;
    value = value.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
    value ^= value >> 33;
    value = value.wrapping_mul(0xC4CE_B9FE_1A85_EC53);
    value ^ (value >> 33)
}

fn benchmark_page(path: &Path, items: usize, workload: &Workload) -> BenchResult<CandidateReport> {
    let build_started = Instant::now();
    build_page_file(path, &workload.buckets)?;
    let build_elapsed = build_started.elapsed();

    let reopen_started = Instant::now();
    let mut store = PageStore::open(path)?;
    let reopen_elapsed = reopen_started.elapsed();
    let rss_after_reopen = current_rss_bytes();
    verify_store(&mut store, workload)?;
    let timing = time_store(&mut store, workload, rss_after_reopen)?;
    report_candidate(
        "paged_file",
        path,
        items,
        build_elapsed,
        reopen_elapsed,
        timing,
    )
}

fn benchmark_mmap(path: &Path, items: usize, workload: &Workload) -> BenchResult<CandidateReport> {
    let build_started = Instant::now();
    build_page_file(path, &workload.buckets)?;
    let build_elapsed = build_started.elapsed();

    let reopen_started = Instant::now();
    let mut store = MmapStore::open(path)?;
    let reopen_elapsed = reopen_started.elapsed();
    let rss_after_reopen = current_rss_bytes();
    verify_store(&mut store, workload)?;
    let timing = time_store(&mut store, workload, rss_after_reopen)?;
    report_candidate(
        "mmap_read_only",
        path,
        items,
        build_elapsed,
        reopen_elapsed,
        timing,
    )
}

fn benchmark_redb(path: &Path, items: usize, workload: &Workload) -> BenchResult<CandidateReport> {
    let build_started = Instant::now();
    build_redb(path, &workload.buckets)?;
    let build_elapsed = build_started.elapsed();

    let reopen_started = Instant::now();
    let mut store = RedbStore::open(path)?;
    let reopen_elapsed = reopen_started.elapsed();
    let rss_after_reopen = current_rss_bytes();
    verify_store(&mut store, workload)?;
    let timing = time_store(&mut store, workload, rss_after_reopen)?;
    report_candidate("redb", path, items, build_elapsed, reopen_elapsed, timing)
}

#[derive(Debug, Clone, Copy)]
struct Timing {
    p50: Duration,
    p95: Duration,
    p99: Duration,
    batch_qps: f64,
    rss_after_reopen: Option<u64>,
}

fn time_store(
    store: &mut impl QueryStore,
    workload: &Workload,
    rss_after_reopen: Option<u64>,
) -> BenchResult<Timing> {
    let mut latencies = Vec::with_capacity(workload.query_buckets.len());
    for buckets in &workload.query_buckets {
        let started = Instant::now();
        let result = store.query(buckets)?;
        drop(std::hint::black_box(result));
        latencies.push(started.elapsed());
    }
    latencies.sort_unstable();

    let batch_started = Instant::now();
    for buckets in &workload.query_buckets {
        let result = store.query(buckets)?;
        drop(std::hint::black_box(result));
    }
    let batch_elapsed = batch_started.elapsed();

    Ok(Timing {
        p50: percentile_duration(&latencies, 50),
        p95: percentile_duration(&latencies, 95),
        p99: percentile_duration(&latencies, 99),
        batch_qps: rate(workload.query_buckets.len(), batch_elapsed)?,
        rss_after_reopen,
    })
}

fn verify_store(store: &mut impl QueryStore, workload: &Workload) -> BenchResult<()> {
    for (query_index, (buckets, expected)) in workload
        .query_buckets
        .iter()
        .zip(&workload.expected)
        .enumerate()
    {
        let actual = store.query(buckets)?;
        if actual != *expected {
            return Err(format!(
                "candidate parity failed for query {query_index}: expected {expected:?}, got {actual:?}"
            )
            .into());
        }
    }
    Ok(())
}

fn report_candidate(
    name: &str,
    path: &Path,
    items: usize,
    build_elapsed: Duration,
    reopen_elapsed: Duration,
    timing: Timing,
) -> BenchResult<CandidateReport> {
    let file_bytes = fs::metadata(path)?.len();
    Ok(CandidateReport {
        name: name.to_owned(),
        build_ms: duration_ms(build_elapsed),
        build_items_per_second: rate(items, build_elapsed)?,
        reopen_ms: duration_ms(reopen_elapsed),
        file_bytes,
        bytes_per_item: bytes_per_item(file_bytes, items)?,
        scalar_p50_ms: duration_ms(timing.p50),
        scalar_p95_ms: duration_ms(timing.p95),
        scalar_p99_ms: duration_ms(timing.p99),
        batch_queries_per_second: timing.batch_qps,
        rss_after_reopen_bytes: timing.rss_after_reopen,
    })
}

fn duration_ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

fn percentile_duration(sorted: &[Duration], percent: usize) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let rank = sorted.len().saturating_mul(percent).saturating_add(99) / 100;
    sorted[rank.max(1).saturating_sub(1).min(sorted.len() - 1)]
}

fn rate(count: usize, elapsed: Duration) -> BenchResult<f64> {
    let seconds = elapsed.as_secs_f64();
    if seconds == 0.0 {
        return Ok(0.0);
    }
    Ok(f64::from(u32::try_from(count)?) / seconds)
}

#[allow(clippy::cast_precision_loss)]
fn bytes_per_item(file_bytes: u64, items: usize) -> BenchResult<f64> {
    Ok(file_bytes as f64 / f64::from(u32::try_from(items)?))
}

fn build_page_file(path: &Path, buckets: &BTreeMap<BucketKey, Vec<u64>>) -> BenchResult<()> {
    let pages = encode_pages(buckets)?;
    let page_count = u32::try_from(pages.len())?;
    let directory_bytes = pages
        .len()
        .checked_mul(PAGE_DIRECTORY_BYTES)
        .ok_or_else(|| invalid_data("page directory size overflow"))?;
    let payload_start = PAGE_HEADER_BYTES
        .checked_add(directory_bytes)
        .ok_or_else(|| invalid_data("page payload offset overflow"))?;
    let mut offset = u64::try_from(payload_start)?;
    let mut descriptors = Vec::with_capacity(pages.len());
    for page in &pages {
        let length = u32::try_from(page.bytes.len())?;
        descriptors.push(PageDescriptor {
            first: page.first,
            last: page.last,
            offset,
            length,
        });
        offset = offset
            .checked_add(u64::from(length))
            .ok_or_else(|| invalid_data("page file offset overflow"))?;
    }

    let mut file = OpenOptions::new().create_new(true).write(true).open(path)?;
    file.write_all(&PAGE_MAGIC)?;
    file.write_all(&PAGE_VERSION.to_le_bytes())?;
    file.write_all(&page_count.to_le_bytes())?;
    for descriptor in &descriptors {
        write_page_descriptor(&mut file, *descriptor)?;
    }
    for page in pages {
        file.write_all(&page.bytes)?;
    }
    file.flush()?;
    file.sync_all()?;
    Ok(())
}

#[derive(Debug)]
struct EncodedPage {
    first: BucketKey,
    last: BucketKey,
    bytes: Vec<u8>,
}

fn encode_pages(buckets: &BTreeMap<BucketKey, Vec<u64>>) -> BenchResult<Vec<EncodedPage>> {
    let mut pages = Vec::new();
    let mut current = Vec::with_capacity(PAGE_TARGET_BYTES);
    let mut first = None;
    let mut last = None;

    for (key, members) in buckets {
        let record = encode_bucket_record(*key, members)?;
        if !current.is_empty() && current.len().saturating_add(record.len()) > PAGE_TARGET_BYTES {
            pages.push(EncodedPage {
                first: first.ok_or_else(|| invalid_data("page missing first key"))?,
                last: last.ok_or_else(|| invalid_data("page missing last key"))?,
                bytes: std::mem::take(&mut current),
            });
            current = Vec::with_capacity(PAGE_TARGET_BYTES.max(record.len()));
            first = None;
        }
        first.get_or_insert(*key);
        last = Some(*key);
        current.extend_from_slice(&record);
    }

    if !current.is_empty() {
        pages.push(EncodedPage {
            first: first.ok_or_else(|| invalid_data("final page missing first key"))?,
            last: last.ok_or_else(|| invalid_data("final page missing last key"))?,
            bytes: current,
        });
    }
    Ok(pages)
}

fn encode_bucket_record(key: BucketKey, members: &[u64]) -> BenchResult<Vec<u8>> {
    let count = u32::try_from(members.len())?;
    let member_bytes = members
        .len()
        .checked_mul(8)
        .ok_or_else(|| invalid_data("bucket member byte length overflow"))?;
    let capacity = BUCKET_RECORD_HEADER_BYTES
        .checked_add(member_bytes)
        .ok_or_else(|| invalid_data("bucket record length overflow"))?;
    let mut output = Vec::with_capacity(capacity);
    output.extend_from_slice(&key.band.to_le_bytes());
    output.extend_from_slice(&key.hash.to_le_bytes());
    output.extend_from_slice(&count.to_le_bytes());
    for member in members {
        output.extend_from_slice(&member.to_le_bytes());
    }
    Ok(output)
}

fn write_page_descriptor(writer: &mut impl Write, descriptor: PageDescriptor) -> io::Result<()> {
    writer.write_all(&descriptor.first.band.to_le_bytes())?;
    writer.write_all(&descriptor.first.hash.to_le_bytes())?;
    writer.write_all(&descriptor.last.band.to_le_bytes())?;
    writer.write_all(&descriptor.last.hash.to_le_bytes())?;
    writer.write_all(&descriptor.offset.to_le_bytes())?;
    writer.write_all(&descriptor.length.to_le_bytes())?;
    writer.write_all(&0_u32.to_le_bytes())?;
    Ok(())
}

fn read_page_directory(file: &mut File) -> BenchResult<Vec<PageDescriptor>> {
    let file_length = file.metadata()?.len();
    let mut header = [0_u8; PAGE_HEADER_BYTES];
    file.seek(SeekFrom::Start(0))?;
    file.read_exact(&mut header)?;
    let page_count = usize::try_from(read_u32(&header, 12)?)?;
    let directory_bytes = page_count
        .checked_mul(PAGE_DIRECTORY_BYTES)
        .ok_or_else(|| invalid_data("page directory size overflow"))?;
    let total = PAGE_HEADER_BYTES
        .checked_add(directory_bytes)
        .ok_or_else(|| invalid_data("page directory offset overflow"))?;
    let mut bytes = vec![0_u8; total];
    bytes[..PAGE_HEADER_BYTES].copy_from_slice(&header);
    file.read_exact(&mut bytes[PAGE_HEADER_BYTES..])?;
    parse_page_directory(&bytes, file_length)
}

fn parse_page_directory(bytes: &[u8], file_length: u64) -> BenchResult<Vec<PageDescriptor>> {
    if bytes.get(..8) != Some(PAGE_MAGIC.as_slice()) {
        return Err(invalid_data("invalid paged benchmark magic").into());
    }
    if read_u32(bytes, 8)? != PAGE_VERSION {
        return Err(invalid_data("unsupported paged benchmark version").into());
    }
    let page_count = usize::try_from(read_u32(bytes, 12)?)?;
    let directory_bytes = page_count
        .checked_mul(PAGE_DIRECTORY_BYTES)
        .ok_or_else(|| invalid_data("page directory length overflow"))?;
    let directory_end = PAGE_HEADER_BYTES
        .checked_add(directory_bytes)
        .ok_or_else(|| invalid_data("page directory offset overflow"))?;
    if bytes.len() < directory_end {
        return Err(invalid_data("truncated paged benchmark directory").into());
    }

    let mut pages = Vec::with_capacity(page_count);
    let mut previous_end = u64::try_from(directory_end)?;
    let mut previous_last = None;
    for index in 0..page_count {
        let start = PAGE_HEADER_BYTES + index * PAGE_DIRECTORY_BYTES;
        let entry = &bytes[start..start + PAGE_DIRECTORY_BYTES];
        if read_u32(entry, 36)? != 0 {
            return Err(invalid_data("nonzero page directory reserved bytes").into());
        }
        let descriptor = PageDescriptor {
            first: BucketKey {
                band: read_u32(entry, 0)?,
                hash: read_u64(entry, 4)?,
            },
            last: BucketKey {
                band: read_u32(entry, 12)?,
                hash: read_u64(entry, 16)?,
            },
            offset: read_u64(entry, 24)?,
            length: read_u32(entry, 32)?,
        };
        if descriptor.first > descriptor.last {
            return Err(invalid_data("page key range is reversed").into());
        }
        if descriptor.offset < previous_end {
            return Err(invalid_data("page ranges overlap or are out of order").into());
        }
        if let Some(last) = previous_last {
            if last >= descriptor.first {
                return Err(invalid_data("page key ranges overlap").into());
            }
        }
        let end = descriptor
            .offset
            .checked_add(u64::from(descriptor.length))
            .ok_or_else(|| invalid_data("page range overflow"))?;
        if end > file_length {
            return Err(invalid_data("page range exceeds file length").into());
        }
        previous_end = end;
        previous_last = Some(descriptor.last);
        pages.push(descriptor);
    }
    if previous_end != file_length {
        return Err(invalid_data("paged benchmark file has trailing or missing page bytes").into());
    }
    Ok(pages)
}

fn find_page(pages: &[PageDescriptor], key: BucketKey) -> Option<PageDescriptor> {
    let index = pages.partition_point(|page| page.last < key);
    pages
        .get(index)
        .copied()
        .filter(|page| page.first <= key && key <= page.last)
}

fn read_page(file: &mut File, descriptor: PageDescriptor) -> BenchResult<Vec<u8>> {
    file.seek(SeekFrom::Start(descriptor.offset))?;
    let mut bytes = vec![0_u8; usize::try_from(descriptor.length)?];
    file.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn collect_bucket(
    page: &[u8],
    target: BucketKey,
    candidates: &mut BTreeSet<u64>,
) -> BenchResult<()> {
    let mut cursor = 0;
    while cursor < page.len() {
        let header_end = cursor
            .checked_add(BUCKET_RECORD_HEADER_BYTES)
            .ok_or_else(|| invalid_data("bucket record header overflow"))?;
        let header = page
            .get(cursor..header_end)
            .ok_or_else(|| invalid_data("truncated bucket record header"))?;
        let key = BucketKey {
            band: read_u32(header, 0)?,
            hash: read_u64(header, 4)?,
        };
        let count = usize::try_from(read_u32(header, 12)?)?;
        let member_bytes = count
            .checked_mul(8)
            .ok_or_else(|| invalid_data("bucket member length overflow"))?;
        let record_end = header_end
            .checked_add(member_bytes)
            .ok_or_else(|| invalid_data("bucket record range overflow"))?;
        let members = page
            .get(header_end..record_end)
            .ok_or_else(|| invalid_data("truncated bucket members"))?;
        if key == target {
            collect_members(members, candidates)?;
            return Ok(());
        }
        if key > target {
            return Ok(());
        }
        cursor = record_end;
    }
    Ok(())
}

fn collect_members(bytes: &[u8], candidates: &mut BTreeSet<u64>) -> BenchResult<()> {
    let mut chunks = bytes.chunks_exact(8);
    for chunk in &mut chunks {
        candidates.insert(u64::from_le_bytes(chunk.try_into()?));
    }
    if !chunks.remainder().is_empty() {
        return Err(invalid_data("bucket member bytes are not aligned to u64").into());
    }
    Ok(())
}

fn build_redb(path: &Path, buckets: &BTreeMap<BucketKey, Vec<u64>>) -> BenchResult<()> {
    let database = Database::create(path)?;
    let transaction = database.begin_write()?;
    {
        let mut table = transaction.open_table(REDB_BUCKETS)?;
        for (key, members) in buckets {
            let encoded_key = key.encode();
            let encoded_members = encode_members(members)?;
            table.insert(encoded_key.as_slice(), encoded_members.as_slice())?;
        }
    }
    transaction.commit()?;
    drop(database);
    Ok(())
}

fn encode_members(members: &[u64]) -> BenchResult<Vec<u8>> {
    let capacity = members
        .len()
        .checked_mul(8)
        .ok_or_else(|| invalid_data("member encoding length overflow"))?;
    let mut output = Vec::with_capacity(capacity);
    for member in members {
        output.extend_from_slice(&member.to_le_bytes());
    }
    Ok(output)
}

fn read_u32(bytes: &[u8], offset: usize) -> io::Result<u32> {
    let end = offset
        .checked_add(4)
        .ok_or_else(|| invalid_data("u32 offset overflow"))?;
    let raw: [u8; 4] = bytes
        .get(offset..end)
        .ok_or_else(|| invalid_data("truncated u32"))?
        .try_into()
        .map_err(|_| invalid_data("invalid u32 width"))?;
    Ok(u32::from_le_bytes(raw))
}

fn read_u64(bytes: &[u8], offset: usize) -> io::Result<u64> {
    let end = offset
        .checked_add(8)
        .ok_or_else(|| invalid_data("u64 offset overflow"))?;
    let raw: [u8; 8] = bytes
        .get(offset..end)
        .ok_or_else(|| invalid_data("truncated u64"))?
        .try_into()
        .map_err(|_| invalid_data("invalid u64 width"))?;
    Ok(u64::from_le_bytes(raw))
}

fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn temporary_root() -> BenchResult<PathBuf> {
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    Ok(env::temp_dir().join(format!(
        "pari-storage-shootout-{}-{nonce}",
        std::process::id()
    )))
}

#[cfg(target_os = "linux")]
fn current_rss_bytes() -> Option<u64> {
    let status = fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|line| line.starts_with("VmRSS:"))?;
    let mut fields = line.split_whitespace();
    if fields.next()? != "VmRSS:" {
        return None;
    }
    let kibibytes = fields.next()?.parse::<u64>().ok()?;
    if fields.next()? != "kB" {
        return None;
    }
    kibibytes.checked_mul(1024)
}

#[cfg(not(target_os = "linux"))]
fn current_rss_bytes() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::{
        build_page_file, build_redb, build_workload, temporary_root, MmapStore, PageStore,
        RedbStore,
    };

    #[test]
    fn all_storage_candidates_match_reference_candidates() {
        let root = temporary_root().expect("temporary root");
        std::fs::create_dir_all(&root).expect("create root");
        let workload = build_workload(64, 16).expect("workload");

        let page_path = root.join("page.idx");
        build_page_file(&page_path, &workload.buckets).expect("build pages");
        let mut page = PageStore::open(&page_path).expect("open page store");
        super::verify_store(&mut page, &workload).expect("page parity");

        let mmap_path = root.join("mmap.idx");
        build_page_file(&mmap_path, &workload.buckets).expect("build mmap file");
        let mut mmap = MmapStore::open(&mmap_path).expect("open mmap store");
        super::verify_store(&mut mmap, &workload).expect("mmap parity");

        let redb_path = root.join("embedded.redb");
        build_redb(&redb_path, &workload.buckets).expect("build redb");
        let mut redb = RedbStore::open(&redb_path).expect("open redb");
        super::verify_store(&mut redb, &workload).expect("redb parity");

        drop(redb);
        drop(mmap);
        drop(page);
        let _ = std::fs::remove_dir_all(root);
    }
}
