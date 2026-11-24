#![forbid(unsafe_code)]

use std::{
    collections::HashMap,
    error::Error,
    ffi::OsString,
    fmt,
    fs::{self, File},
    io::{self, BufRead, BufReader, BufWriter, Write},
    path::{Path, PathBuf},
    process::{self, ExitCode},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use clap_complete::{generate, Shell};
use pari_core::{MinHash32, MinHash64, AFFINE32_SCHEME, AFFINE64_SCHEME};
use pari_format::{
    decode_bucket_segment, read_bucket_members, BucketDirectory, FileLayout, SectionKind,
    SignatureScheme,
};
use pari_index::{
    plan_lsh, BucketDistribution, DuplicateGroup, LshIndex32, LshIndex64, LshPlan, LshPlanOptions,
    QueryMetrics, StorageMode,
};
use pari_store::{
    PersistentIndex32, PersistentIndex64, PersistentIndexOptions, ScoreFilter, ScoredCandidate,
    StoreStats,
};
use pari_store_build::{build_external, BuildOptions};
use pari_store_lazy::LazyIndex;
use same_file::{is_same_file, Handle};
use serde::{Deserialize, Serialize};

/// Write one line to stdout, propagating write failures such as a closed pipe
/// instead of panicking like `println!`.
macro_rules! outln {
    ($($arg:tt)*) => {
        writeln!(io::stdout().lock(), $($arg)*)?
    };
}

#[derive(Debug, Parser)]
#[command(
    name = "pari",
    version,
    about = "Similarity indexing and deduplication for large datasets"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Build a persistent local index from JSONL records.
    Index(IndexArgs),
    /// Query a persistent index with JSONL records.
    Search(SearchArgs),
    /// Emit candidate pairs or connected duplicate groups from JSONL records.
    Dedup(DedupArgs),
    /// Recommend LSH parameters, capacity, and storage from analytical models.
    Plan(PlanArgs),
    /// Explain an existing index without scanning bucket memberships.
    Explain(ExplainArgs),
    /// Print index metadata and storage statistics.
    Stats(StatsArgs),
    /// Validate index structure and all persisted checksums.
    Verify(VerifyArgs),
    /// Rewrite an index as one snapshot, folding in appended commits.
    Compact(CompactArgs),
    /// Write a query-ready copy of a compacted index in bounded memory.
    BuildReadOnly(BuildReadOnlyArgs),
    /// Generate shell completion from this command definition.
    Completion(CompletionArgs),
}

#[derive(Debug, clap::Args)]
struct IndexArgs {
    /// JSONL input path, or '-' for stdin.
    #[arg(short, long, default_value = "-")]
    input: String,
    /// Destination `.pari` index file.
    #[arg(short, long)]
    output: PathBuf,
    #[arg(long, default_value_t = 0.8)]
    threshold: f64,
    #[arg(long, default_value_t = 128)]
    num_perm: usize,
    #[arg(long, default_value_t = 1)]
    seed: u64,
    /// Signature family. Omission preserves the affine32 format and behavior.
    #[arg(long, value_enum, default_value_t = CliSignatureScheme::PariAffine32V1)]
    signature_scheme: CliSignatureScheme,
    /// Records committed per batch. Smaller values reduce uncommitted overlay memory.
    #[arg(long, default_value_t = 10_000)]
    batch_size: usize,
    /// Store full signatures so `search --scores` can report similarity.
    #[arg(long)]
    store_signatures: bool,
    /// Print the final summary as JSON.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    progress: ProgressArgs,
}

#[derive(Debug, clap::Args)]
struct SearchArgs {
    /// Existing `.pari` index file.
    #[arg(long)]
    index: PathBuf,
    /// Open a committed index without loading metadata used for updates.
    #[arg(long)]
    read_only: bool,
    /// JSONL query path, or '-' for stdin.
    #[arg(short, long, default_value = "-")]
    input: String,
    /// Emit JSONL results instead of tab-separated text.
    #[arg(long)]
    json: bool,
    /// Report estimated similarity per candidate, most similar first. Requires
    /// an index built with `--store-signatures`.
    #[arg(long)]
    scores: bool,
    /// Keep candidates with estimated similarity at least this value. Implies `--scores`.
    #[arg(long)]
    min_similarity: Option<f64>,
    /// Keep at most this many candidates per query. Implies `--scores`.
    #[arg(long)]
    top_k: Option<usize>,
    #[command(flatten)]
    progress: ProgressArgs,
}

impl SearchArgs {
    fn score_filter(&self) -> Option<ScoreFilter> {
        if !self.scores && self.min_similarity.is_none() && self.top_k.is_none() {
            return None;
        }
        let mut filter = ScoreFilter::new();
        if let Some(threshold) = self.min_similarity {
            filter = filter.min_similarity(threshold);
        }
        if let Some(count) = self.top_k {
            filter = filter.top_k(count);
        }
        Some(filter)
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum DedupOutput {
    Pairs,
    Groups,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum CliSignatureScheme {
    #[value(name = "pari-affine32-v1")]
    PariAffine32V1,
    #[value(name = "pari-affine64-v1")]
    PariAffine64V1,
}

#[derive(Debug, clap::Args)]
struct DedupArgs {
    /// JSONL input path, or '-' for stdin.
    #[arg(short, long, default_value = "-")]
    input: String,
    /// Output path, or '-' for stdout.
    #[arg(short, long, default_value = "-")]
    output: String,
    #[arg(long, default_value_t = 0.8)]
    threshold: f64,
    #[arg(long, default_value_t = 128)]
    num_perm: usize,
    #[arg(long, default_value_t = 1)]
    seed: u64,
    /// Signature family. Omission preserves affine32 behavior.
    #[arg(long, value_enum, default_value_t = CliSignatureScheme::PariAffine32V1)]
    signature_scheme: CliSignatureScheme,
    #[arg(long, value_enum, default_value_t = DedupOutput::Groups)]
    emit: DedupOutput,
    /// Minimum component size when emitting groups.
    #[arg(long, default_value_t = 2)]
    min_size: usize,
    /// Keep only candidate pairs whose estimated similarity is at least this
    /// value in `[0, 1]`. Pairs output then includes each pair's similarity.
    #[arg(long)]
    min_similarity: Option<f64>,
    /// Emit JSONL instead of tab-separated text.
    #[arg(long)]
    json: bool,
    /// Records indexed per atomic in-memory batch.
    #[arg(long, default_value_t = 10_000)]
    batch_size: usize,
    #[command(flatten)]
    progress: ProgressArgs,
}

#[derive(Debug, clap::Args)]
struct PlanArgs {
    /// Expected number of indexed items.
    #[arg(long)]
    items: u64,
    #[arg(long, default_value_t = 0.8)]
    threshold: f64,
    #[arg(long, default_value_t = 128)]
    num_perm: usize,
    /// Local resident-memory budget in MiB.
    #[arg(long)]
    memory_budget_mib: Option<u64>,
    /// Storage preference: auto, memory, persistent, lazy, or redis.
    #[arg(long, default_value = "auto")]
    storage: StorageMode,
    /// Emit the complete plan as JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, clap::Args)]
struct ExplainArgs {
    /// Existing `.pari` index file.
    #[arg(short, long)]
    index: PathBuf,
    /// Emit the complete explanation as JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, clap::Args)]
struct StatsArgs {
    #[arg(short, long)]
    index: PathBuf,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, clap::Args)]
struct CompactArgs {
    #[arg(short, long)]
    index: PathBuf,
}

#[derive(Debug, clap::Args)]
struct BuildReadOnlyArgs {
    /// Committed source index. Appended commits are folded in.
    #[arg(short, long)]
    index: PathBuf,
    /// Destination file. It must not exist yet.
    #[arg(short, long)]
    output: PathBuf,
    /// Always re-sort every bucket record instead of reusing the source's
    /// verified bucket sections.
    #[arg(long)]
    no_reuse_buckets: bool,
    /// Records sorted in memory before spilling a run to disk.
    #[arg(long, default_value_t = 262_144)]
    max_buffer_records: usize,
    /// Print the summary as JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, clap::Args)]
struct VerifyArgs {
    #[arg(short, long)]
    index: PathBuf,
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    progress: ProgressArgs,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ProgressFormat {
    Human,
    Json,
}

#[derive(Debug, Clone, clap::Args)]
struct ProgressArgs {
    /// Emit progress to stderr. Optionally choose `human` or `json`.
    #[arg(long, value_enum, num_args = 0..=1, default_missing_value = "human")]
    progress: Option<ProgressFormat>,
    /// Record interval for search and verification progress.
    #[arg(long, default_value_t = 1_000)]
    progress_every: usize,
}

#[derive(Debug, clap::Args)]
struct CompletionArgs {
    #[arg(value_enum)]
    shell: Shell,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    key: u64,
    #[serde(default)]
    values: Option<Vec<String>>,
    #[serde(default)]
    signature: Option<Vec<u64>>,
    #[serde(default)]
    scheme: Option<String>,
    /// Optional producer seed metadata, validated when present.
    #[serde(default)]
    seed: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct QueryRecord {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    values: Option<Vec<String>>,
    #[serde(default)]
    signature: Option<Vec<u64>>,
    #[serde(default)]
    scheme: Option<String>,
    /// Optional producer seed metadata, validated when present.
    #[serde(default)]
    seed: Option<u64>,
}

#[derive(Debug, Serialize)]
struct SearchResult<'a> {
    query: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: &'a Option<String>,
    candidates: Vec<u64>,
    /// Estimated similarities aligned with `candidates`, present only in scored mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    scores: Option<Vec<f64>>,
}

/// Candidate keys plus optional aligned similarity scores.
type SearchRow = (Vec<u64>, Option<Vec<f64>>);

fn split_scored(ranked: Vec<ScoredCandidate>) -> SearchRow {
    let (keys, scores) = ranked
        .into_iter()
        .map(|candidate| (candidate.key, candidate.similarity))
        .unzip();
    (keys, Some(scores))
}

fn format_candidates(candidates: &[u64], scores: Option<&[f64]>) -> String {
    match scores {
        None => join_u64(candidates),
        Some(scores) => candidates
            .iter()
            .zip(scores)
            .map(|(key, score)| format!("{key}:{score:.4}"))
            .collect::<Vec<_>>()
            .join(","),
    }
}

#[derive(Debug, Serialize)]
struct IndexSummary {
    items: usize,
    file_bytes: u64,
    bands: usize,
    rows: usize,
}

#[derive(Debug, Serialize)]
struct PairResult {
    left: u64,
    right: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    similarity: Option<f64>,
}

/// Signatures kept for `dedup --min-similarity`, keyed by record key.
#[derive(Default)]
struct SimilarityCheck {
    minimum: Option<f64>,
    signatures: HashMap<u64, Vec<u64>>,
}

impl SimilarityCheck {
    fn new(minimum: Option<f64>) -> Self {
        Self {
            minimum,
            signatures: HashMap::new(),
        }
    }

    fn remember<T: Copy + Into<u64>>(&mut self, key: u64, signature: &[T]) {
        if self.minimum.is_some() {
            self.signatures
                .insert(key, signature.iter().map(|value| (*value).into()).collect());
        }
    }

    fn similarity(&self, left: u64, right: u64) -> f64 {
        match (self.signatures.get(&left), self.signatures.get(&right)) {
            (Some(left), Some(right)) if !left.is_empty() && left.len() == right.len() => {
                let matches = left.iter().zip(right).filter(|(a, b)| a == b).count();
                u64_to_f64(u64::try_from(matches).unwrap_or(u64::MAX))
                    / u64_to_f64(u64::try_from(left.len()).unwrap_or(u64::MAX))
            }
            _ => 0.0,
        }
    }

    fn accepts(&self, left: u64, right: u64) -> bool {
        self.minimum
            .map_or(true, |minimum| self.similarity(left, right) >= minimum)
    }

    fn scored_pairs(
        &self,
        pairs: impl IntoIterator<Item = (u64, u64)>,
    ) -> Vec<(u64, u64, Option<f64>)> {
        pairs
            .into_iter()
            .filter_map(|(left, right)| match self.minimum {
                None => Some((left, right, None)),
                Some(minimum) => {
                    let similarity = self.similarity(left, right);
                    (similarity >= minimum).then_some((left, right, Some(similarity)))
                }
            })
            .collect()
    }
}

#[derive(Debug, Serialize)]
struct GroupResult<'a> {
    representative: u64,
    members: &'a [u64],
}

#[derive(Debug, Serialize)]
struct VerifyResult {
    valid: bool,
    sections: usize,
    bucket_sections: usize,
    buckets: usize,
    members_checked: u64,
    delta_generations: usize,
}

#[derive(Debug, Serialize)]
struct ProgressEvent {
    schema_version: u32,
    phase: &'static str,
    completed: u64,
    total: Option<u64>,
    elapsed_ms: f64,
    rate_per_second: f64,
    final_event: bool,
    candidates: Option<u64>,
    candidate_rate: Option<f64>,
}

struct ProgressReporter {
    format: ProgressFormat,
    every: usize,
    started: Instant,
}

struct IndexOutputTransaction {
    final_path: PathBuf,
    staged_path: PathBuf,
    active: bool,
}

impl IndexOutputTransaction {
    fn begin(final_path: &Path) -> Result<Self, Box<dyn Error>> {
        match fs::symlink_metadata(final_path) {
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("index path {} already exists", final_path.display()),
                )
                .into());
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }

        let file_name = final_path
            .file_name()
            .ok_or("index output must identify a file")?;
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let mut staged_name = OsString::from(".");
        staged_name.push(file_name);
        staged_name.push(format!(".pari-cli-{}-{nonce}.tmp", process::id()));
        Ok(Self {
            final_path: final_path.to_path_buf(),
            staged_path: final_path.with_file_name(staged_name),
            active: true,
        })
    }

    fn staged_path(&self) -> &Path {
        &self.staged_path
    }

    fn publish(self) -> Result<(), Box<dyn Error>> {
        self.publish_with(|path| fs::remove_file(path), sync_parent_directory)
    }

    fn publish_with<R, S>(mut self, remove_staged: R, sync_parent: S) -> Result<(), Box<dyn Error>>
    where
        R: FnOnce(&Path) -> io::Result<()>,
        S: FnOnce(&Path) -> io::Result<()>,
    {
        let identity = match Handle::from_path(&self.staged_path) {
            Ok(identity) => identity,
            Err(error) => return Err(self.abort(error.into())),
        };
        if let Err(error) = fs::hard_link(&self.staged_path, &self.final_path) {
            let publication = io::Error::new(
                error.kind(),
                format!(
                    "failed to publish index {} without replacement: {error}",
                    self.final_path.display()
                ),
            );
            return Err(self.abort(publication.into()));
        }

        if let Err(error) = remove_staged(&self.staged_path) {
            let cleanup = io::Error::new(
                error.kind(),
                format!(
                    "failed to remove staged index {} after publication: {error}",
                    self.staged_path.display()
                ),
            );
            return Err(self.recover_after_publication(&identity, cleanup.into()));
        }
        if let Err(error) = remove_file_if_exists(&store_temporary_path(&self.staged_path)) {
            let cleanup = io::Error::new(
                error.kind(),
                format!("failed to remove staged index companion after publication: {error}"),
            );
            return Err(self.recover_after_publication(&identity, cleanup.into()));
        }
        if let Err(error) = sync_parent(&self.final_path) {
            let durability = io::Error::new(
                error.kind(),
                format!(
                    "failed to sync index output directory after publishing {}: {error}",
                    self.final_path.display()
                ),
            );
            return Err(self.recover_after_publication(&identity, durability.into()));
        }
        self.active = false;
        Ok(())
    }

    fn recover_after_publication(
        mut self,
        identity: &Handle,
        original: Box<dyn Error>,
    ) -> Box<dyn Error> {
        let rollback = self
            .rollback_final(identity)
            .err()
            .map(|error| error.to_string());
        let cleanup = self
            .remove_staged_files()
            .err()
            .map(|error| error.to_string());
        if rollback.is_none() && cleanup.is_none() {
            return original;
        }
        Box::new(IndexRecoveryError {
            original,
            rollback,
            cleanup,
        })
    }

    fn rollback_final(&self, identity: &Handle) -> io::Result<()> {
        let current = match Handle::from_path(&self.final_path) {
            Ok(current) => current,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        if &current != identity {
            return Ok(());
        }
        fs::remove_file(&self.final_path)?;
        sync_parent_directory(&self.final_path)
    }

    fn abort(mut self, original: Box<dyn Error>) -> Box<dyn Error> {
        match self.remove_staged_files() {
            Ok(()) => original,
            Err(cleanup) => Box::new(IndexCleanupError {
                staged_path: self.staged_path.clone(),
                original,
                cleanup,
            }),
        }
    }

    fn remove_staged_files(&mut self) -> io::Result<()> {
        let mut first_error = None;
        for path in [&self.staged_path, &store_temporary_path(&self.staged_path)] {
            if let Err(error) = remove_file_if_exists(path) {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
        if let Some(error) = first_error {
            return Err(error);
        }
        self.active = false;
        Ok(())
    }
}

impl Drop for IndexOutputTransaction {
    fn drop(&mut self) {
        if self.active {
            let _ = self.remove_staged_files();
        }
    }
}

#[derive(Debug)]
struct IndexCleanupError {
    staged_path: PathBuf,
    original: Box<dyn Error>,
    cleanup: io::Error,
}

impl fmt::Display for IndexCleanupError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}; also failed to clean staged index {}: {}",
            self.original,
            self.staged_path.display(),
            self.cleanup
        )
    }
}

impl Error for IndexCleanupError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.original.as_ref())
    }
}

#[derive(Debug)]
struct IndexRecoveryError {
    original: Box<dyn Error>,
    rollback: Option<String>,
    cleanup: Option<String>,
}

impl fmt::Display for IndexRecoveryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.original)?;
        if let Some(rollback) = &self.rollback {
            write!(
                formatter,
                "; also failed to roll back published index: {rollback}"
            )?;
        }
        if let Some(cleanup) = &self.cleanup {
            write!(formatter, "; also failed to clean staged index: {cleanup}")?;
        }
        Ok(())
    }
}

impl Error for IndexRecoveryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.original.as_ref())
    }
}

fn remove_file_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn store_temporary_path(path: &Path) -> PathBuf {
    let mut temporary = path.as_os_str().to_os_string();
    temporary.push(".tmp");
    PathBuf::from(temporary)
}

#[cfg_attr(not(unix), allow(clippy::unnecessary_wraps))]
fn sync_parent_directory(path: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        let _ = path;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        File::open(parent)?.sync_all()
    }
}

impl ProgressReporter {
    fn new(args: &ProgressArgs) -> Result<Option<Self>, Box<dyn Error>> {
        let Some(format) = args.progress else {
            return Ok(None);
        };
        if args.progress_every == 0 {
            return Err("--progress-every must be positive".into());
        }
        Ok(Some(Self {
            format,
            every: args.progress_every,
            started: Instant::now(),
        }))
    }

    fn is_due(&self, completed: usize) -> bool {
        completed > 0 && completed % self.every == 0
    }

    fn emit(
        &self,
        phase: &'static str,
        completed: usize,
        total: Option<usize>,
        final_event: bool,
        candidates: Option<u64>,
        candidate_rate: Option<f64>,
    ) -> Result<(), Box<dyn Error>> {
        let elapsed = self.started.elapsed();
        let seconds = elapsed.as_secs_f64();
        let completed_u64 = u64::try_from(completed)?;
        let event = ProgressEvent {
            schema_version: 1,
            phase,
            completed: completed_u64,
            total: total.map(u64::try_from).transpose()?,
            elapsed_ms: seconds * 1_000.0,
            rate_per_second: if seconds > 0.0 {
                u64_to_f64(completed_u64) / seconds
            } else {
                0.0
            },
            final_event,
            candidates,
            candidate_rate,
        };
        match self.format {
            ProgressFormat::Human => {
                eprintln!(
                    "{phase}: {}{} ({:.1}/s, {:.1} ms)",
                    event.completed,
                    event
                        .total
                        .map_or_else(String::new, |total| format!("/{total}")),
                    event.rate_per_second,
                    event.elapsed_ms
                );
            }
            ProgressFormat::Json => {
                let stderr = io::stderr();
                let mut stderr = stderr.lock();
                serde_json::to_writer(&mut stderr, &event)?;
                writeln!(stderr)?;
            }
        }
        Ok(())
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        // A downstream reader such as `head` closing the pipe is a normal way
        // to stop consuming output, not a failure worth reporting.
        Err(error) if is_broken_pipe(error.as_ref()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("pari: {error}");
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let cli = Cli::parse();
    match &cli.command {
        Commands::Index(args) => index(args),
        Commands::Search(args) => search(args),
        Commands::Dedup(args) => dedup(args),
        Commands::Plan(args) => plan(args),
        Commands::Explain(args) => explain(args),
        Commands::Stats(args) => stats(args),
        Commands::Verify(args) => verify(args),
        Commands::Compact(args) => compact(args),
        Commands::BuildReadOnly(args) => build_read_only(args),
        Commands::Completion(args) => {
            let mut command = Cli::command();
            let name = command.get_name().to_owned();
            let mut script = Vec::new();
            generate(args.shell, &mut command, name, &mut script);
            let mut stdout = io::stdout().lock();
            stdout.write_all(&script)?;
            stdout.flush()?;
            Ok(())
        }
    }
}

fn plan(args: &PlanArgs) -> Result<(), Box<dyn Error>> {
    let mut options =
        LshPlanOptions::new(args.items, args.threshold, args.num_perm).storage_mode(args.storage);
    if let Some(mebibytes) = args.memory_budget_mib {
        let bytes = mebibytes
            .checked_mul(1024 * 1024)
            .ok_or("--memory-budget-mib is too large")?;
        options = options.memory_budget_bytes(bytes);
    }
    print_plan(plan_lsh(options)?, args.json)
}

fn explain(args: &ExplainArgs) -> Result<(), Box<dyn Error>> {
    let explanation = match index_signature_scheme(&args.index)? {
        SignatureScheme::PariAffine32V1 => PersistentIndex32::open(&args.index)?.explain()?,
        SignatureScheme::PariAffine64V1 => PersistentIndex64::open(&args.index)?.explain()?,
    };
    print_plan(explanation, args.json)
}

fn print_plan(plan: LshPlan, json: bool) -> Result<(), Box<dyn Error>> {
    if json {
        outln!("{}", lsh_plan_json(plan));
        return Ok(());
    }

    outln!(
        "model: {} (analytical/model-based, not a measured guarantee)",
        plan.model
    );
    outln!("items: {}", plan.expected_items);
    outln!("threshold: {}", plan.threshold);
    outln!("num perm: {}", plan.num_perm);
    outln!(
        "parameters: {} bands x {} rows ({})",
        plan.params.bands,
        plan.params.rows,
        plan.parameter_source.as_str()
    );
    outln!(
        "permutations: {} used, {} unused",
        plan.used_permutations,
        plan.unused_permutations
    );
    outln!(
        "candidate probability at threshold: {:.6}",
        plan.candidate_probability_at_threshold
    );
    outln!(
        "50% candidate similarity: {:.6}",
        plan.similarity_at_50_percent_candidates
    );
    outln!("false-positive area: {:.6}", plan.false_positive_area);
    outln!("false-negative area: {:.6}", plan.false_negative_area);
    outln!(
        "signature bytes/item: {}",
        plan.sizes.signature_bytes_per_item
    );
    outln!(
        "bucket memberships/item: {}",
        plan.bucket_memberships_per_item
    );
    outln!(
        "modeled index metadata bytes: {}",
        plan.sizes.index_metadata_bytes
    );
    outln!(
        "modeled in-memory index bytes: {}",
        plan.sizes.in_memory_index_bytes
    );
    outln!(
        "modeled persistent index bytes: {}",
        plan.sizes.persistent_index_bytes
    );
    outln!(
        "modeled lazy resident bytes: {}",
        plan.sizes.lazy_resident_bytes
    );
    if let Some(budget) = plan.memory_budget_bytes {
        outln!("memory budget bytes: {budget}");
        outln!(
            "in-memory fits with 50% headroom: {}",
            plan.in_memory_fits_budget.unwrap_or(false)
        );
        outln!(
            "persistent/lazy fits with 50% headroom: {}",
            plan.persistent_fits_budget.unwrap_or(false)
        );
    }
    outln!("requested storage: {}", plan.requested_storage);
    outln!("recommended storage: {}", plan.recommended_storage);
    outln!("recommendation: {}", plan.recommendation_guidance());
    Ok(())
}

fn lsh_plan_json(plan: LshPlan) -> serde_json::Value {
    serde_json::json!({
        "model": plan.model,
        "estimate_semantics": "analytical/model-based, not a measured guarantee",
        "items": plan.expected_items,
        "threshold": plan.threshold,
        "num_perm": plan.num_perm,
        "bands": plan.params.bands,
        "rows": plan.params.rows,
        "parameter_source": plan.parameter_source.as_str(),
        "used_permutations": plan.used_permutations,
        "unused_permutations": plan.unused_permutations,
        "candidate_probability_at_threshold": plan.candidate_probability_at_threshold,
        "similarity_at_50_percent_candidates": plan.similarity_at_50_percent_candidates,
        "false_positive_area": plan.false_positive_area,
        "false_negative_area": plan.false_negative_area,
        "bucket_memberships_per_item": plan.bucket_memberships_per_item,
        "sizes": {
            "signature_bytes_per_item": plan.sizes.signature_bytes_per_item,
            "signature_bytes": plan.sizes.signature_bytes,
            "index_metadata_bytes_per_item": plan.sizes.index_metadata_bytes_per_item,
            "index_metadata_bytes": plan.sizes.index_metadata_bytes,
            "in_memory_index_bytes_per_item": plan.sizes.in_memory_index_bytes_per_item,
            "in_memory_index_bytes": plan.sizes.in_memory_index_bytes,
            "persistent_index_bytes_per_item": plan.sizes.persistent_index_bytes_per_item,
            "persistent_index_bytes": plan.sizes.persistent_index_bytes,
            "lazy_resident_bytes_per_item": plan.sizes.lazy_resident_bytes_per_item,
            "lazy_resident_bytes": plan.sizes.lazy_resident_bytes,
            "in_memory_with_headroom_bytes": plan.sizes.in_memory_with_headroom_bytes,
            "lazy_with_headroom_bytes": plan.sizes.lazy_with_headroom_bytes,
        },
        "memory_budget_bytes": plan.memory_budget_bytes,
        "in_memory_fits_budget": plan.in_memory_fits_budget,
        "persistent_fits_budget": plan.persistent_fits_budget,
        "requested_storage": plan.requested_storage.as_str(),
        "recommended_storage": plan.recommended_storage.as_str(),
        "recommendation_reason": plan.recommendation_reason.as_str(),
        "recommendation": plan.recommendation_guidance(),
    })
}

fn index(args: &IndexArgs) -> Result<(), Box<dyn Error>> {
    if args.batch_size == 0 {
        return Err("--batch-size must be positive".into());
    }
    let transaction = IndexOutputTransaction::begin(&args.output)?;
    let summary = match build_index(args, transaction.staged_path()) {
        Ok(summary) => summary,
        Err(error) => return Err(transaction.abort(error)),
    };
    transaction.publish()?;
    if args.json {
        outln!("{}", serde_json::to_string(&summary)?);
    } else {
        outln!(
            "indexed {} items into {} ({} bytes, {} bands x {} rows)",
            summary.items,
            args.output.display(),
            summary.file_bytes,
            summary.bands,
            summary.rows
        );
    }
    Ok(())
}

fn build_index(args: &IndexArgs, output: &Path) -> Result<IndexSummary, Box<dyn Error>> {
    match args.signature_scheme {
        CliSignatureScheme::PariAffine32V1 => build_index32(args, output),
        CliSignatureScheme::PariAffine64V1 => build_index64(args, output),
    }
}

fn build_index32(args: &IndexArgs, output: &Path) -> Result<IndexSummary, Box<dyn Error>> {
    let mut store = PersistentIndex32::create_with_options(
        output,
        args.threshold,
        args.num_perm,
        args.seed,
        PersistentIndexOptions::new().store_signatures(args.store_signatures),
    )?;
    let reporter = ProgressReporter::new(&args.progress)?;
    let reader = open_reader(&args.input)?;
    let mut batch = Vec::with_capacity(args.batch_size);
    let mut completed = 0_usize;
    for_json_lines(reader, |_line, record: Record| {
        batch.push((
            record.key,
            make_sketch32(
                record.values.as_deref(),
                record.signature.as_deref(),
                record.scheme.as_deref(),
                record.seed,
                args.num_perm,
                args.seed,
            )?,
        ));
        if batch.len() == args.batch_size {
            let batch_len = batch.len();
            store.insert_many(batch.iter().map(|(key, sketch)| (*key, sketch)))?;
            store.sync()?;
            batch.clear();
            completed = completed.saturating_add(batch_len);
            if let Some(reporter) = &reporter {
                reporter.emit("index", completed, None, false, None, None)?;
            }
        }
        Ok(())
    })?;
    if !batch.is_empty() {
        let batch_len = batch.len();
        store.insert_many(batch.iter().map(|(key, sketch)| (*key, sketch)))?;
        completed = completed.saturating_add(batch_len);
    }
    store.sync()?;
    if let Some(reporter) = &reporter {
        reporter.emit("index", completed, None, true, None, None)?;
    }
    let current = store.stats()?;
    let summary = index_summary(&current);
    store.close()?;
    Ok(summary)
}

fn build_index64(args: &IndexArgs, output: &Path) -> Result<IndexSummary, Box<dyn Error>> {
    let mut store = PersistentIndex64::create_with_options(
        output,
        args.threshold,
        args.num_perm,
        args.seed,
        PersistentIndexOptions::new().store_signatures(args.store_signatures),
    )?;
    let reporter = ProgressReporter::new(&args.progress)?;
    let reader = open_reader(&args.input)?;
    let mut batch = Vec::with_capacity(args.batch_size);
    let mut completed = 0_usize;
    for_json_lines(reader, |_line, record: Record| {
        batch.push((
            record.key,
            make_sketch64(
                record.values.as_deref(),
                record.signature.as_deref(),
                record.scheme.as_deref(),
                record.seed,
                args.num_perm,
                args.seed,
            )?,
        ));
        if batch.len() == args.batch_size {
            let batch_len = batch.len();
            store.insert_many(batch.iter().map(|(key, sketch)| (*key, sketch)))?;
            store.sync()?;
            batch.clear();
            completed = completed.saturating_add(batch_len);
            if let Some(reporter) = &reporter {
                reporter.emit("index", completed, None, false, None, None)?;
            }
        }
        Ok(())
    })?;
    if !batch.is_empty() {
        let batch_len = batch.len();
        store.insert_many(batch.iter().map(|(key, sketch)| (*key, sketch)))?;
        completed = completed.saturating_add(batch_len);
    }
    store.sync()?;
    if let Some(reporter) = &reporter {
        reporter.emit("index", completed, None, true, None, None)?;
    }
    let current = store.stats()?;
    let summary = index_summary(&current);
    store.close()?;
    Ok(summary)
}

fn index_summary(current: &StoreStats) -> IndexSummary {
    IndexSummary {
        items: current.items,
        file_bytes: current.file_bytes,
        bands: current.bands,
        rows: current.rows,
    }
}

fn search(args: &SearchArgs) -> Result<(), Box<dyn Error>> {
    let reporter = ProgressReporter::new(&args.progress)?;
    let filter = args.score_filter();

    macro_rules! search_with {
        ($store:expr, $make_sketch:ident) => {{
            let mut store = $store;
            store.set_observability(reporter.is_some());
            search_rows(
                args,
                reporter.as_ref(),
                filter,
                store.len(),
                store.stores_signatures(),
                |query, filter| {
                    let sketch = $make_sketch(
                        query.values.as_deref(),
                        query.signature.as_deref(),
                        query.scheme.as_deref(),
                        query.seed,
                        store.num_perm(),
                        store.seed(),
                    )?;
                    Ok(match filter {
                        Some(filter) => split_scored(store.query_scored(&sketch, filter)?),
                        None => (store.query(&sketch)?, None),
                    })
                },
            )
        }};
    }

    if args.read_only {
        match LazyIndex::open(&args.index)? {
            LazyIndex::Affine32(store) => search_with!(store, make_sketch32),
            LazyIndex::Affine64(store) => search_with!(store, make_sketch64),
        }
    } else {
        match index_signature_scheme(&args.index)? {
            SignatureScheme::PariAffine32V1 => {
                search_with!(PersistentIndex32::open(&args.index)?, make_sketch32)
            }
            SignatureScheme::PariAffine64V1 => {
                search_with!(PersistentIndex64::open(&args.index)?, make_sketch64)
            }
        }
    }
}

fn search_rows(
    args: &SearchArgs,
    reporter: Option<&ProgressReporter>,
    filter: Option<ScoreFilter>,
    items: usize,
    stores_signatures: bool,
    mut query_index_row: impl FnMut(
        &QueryRecord,
        Option<ScoreFilter>,
    ) -> Result<SearchRow, Box<dyn Error>>,
) -> Result<(), Box<dyn Error>> {
    if let Some(filter) = filter {
        filter.validate()?;
        if !stores_signatures {
            return Err(
                "--scores, --min-similarity, and --top-k need an index built with --store-signatures"
                    .into(),
            );
        }
    }
    let reader = open_reader(&args.input)?;
    let mut writer = BufWriter::new(io::stdout().lock());
    let mut query_index = 0_usize;
    let mut candidate_count = 0_u64;
    for_json_lines(reader, |_line, query: QueryRecord| {
        let (candidates, scores) = query_index_row(&query, filter)?;
        if reporter.is_some() {
            candidate_count = candidate_count.saturating_add(u64::try_from(candidates.len())?);
        }
        if args.json {
            serde_json::to_writer(
                &mut writer,
                &SearchResult {
                    query: query_index,
                    id: &query.id,
                    candidates,
                    scores,
                },
            )?;
            writeln!(writer)?;
        } else {
            let id = query.id.as_deref().unwrap_or("-").escape_debug();
            let candidates = format_candidates(&candidates, scores.as_deref());
            writeln!(writer, "{query_index}\t{id}\t{candidates}")?;
        }
        query_index += 1;
        if let Some(reporter) = reporter {
            if reporter.is_due(query_index) {
                let possible = query_index.saturating_mul(items);
                reporter.emit(
                    "search",
                    query_index,
                    None,
                    false,
                    Some(candidate_count),
                    Some(ratio(candidate_count, possible)),
                )?;
            }
        }
        Ok(())
    })?;
    writer.flush()?;
    if let Some(reporter) = reporter {
        let possible = query_index.saturating_mul(items);
        reporter.emit(
            "search",
            query_index,
            None,
            true,
            Some(candidate_count),
            Some(ratio(candidate_count, possible)),
        )?;
    }
    Ok(())
}

fn dedup(args: &DedupArgs) -> Result<(), Box<dyn Error>> {
    if args.min_size == 0 {
        return Err("--min-size must be positive".into());
    }
    if args.batch_size == 0 {
        return Err("--batch-size must be positive".into());
    }
    if let Some(minimum) = args.min_similarity {
        if !minimum.is_finite() || !(0.0..=1.0).contains(&minimum) {
            return Err("--min-similarity must be finite and in [0, 1]".into());
        }
    }
    ensure_distinct_input_output(&args.input, &args.output)?;
    match args.signature_scheme {
        CliSignatureScheme::PariAffine32V1 => dedup32(args),
        CliSignatureScheme::PariAffine64V1 => dedup64(args),
    }
}

fn dedup32(args: &DedupArgs) -> Result<(), Box<dyn Error>> {
    let reporter = ProgressReporter::new(&args.progress)?;
    let mut index = LshIndex32::new(args.threshold, args.num_perm, args.seed)?;
    let mut check = SimilarityCheck::new(args.min_similarity);
    let reader = open_reader(&args.input)?;
    let mut batch = Vec::with_capacity(args.batch_size);
    let mut completed = 0_usize;
    for_json_lines(reader, |_line, record: Record| {
        let sketch = make_sketch32(
            record.values.as_deref(),
            record.signature.as_deref(),
            record.scheme.as_deref(),
            record.seed,
            args.num_perm,
            args.seed,
        )?;
        check.remember(record.key, sketch.signature());
        batch.push((record.key, sketch));
        if batch.len() == args.batch_size {
            let batch_len = batch.len();
            index.insert_many(batch.iter().map(|(key, sketch)| (*key, sketch)))?;
            batch.clear();
            completed = completed.saturating_add(batch_len);
            if let Some(reporter) = &reporter {
                reporter.emit("dedup_index", completed, None, false, None, None)?;
            }
        }
        Ok(())
    })?;
    if !batch.is_empty() {
        let batch_len = batch.len();
        index.insert_many(batch.iter().map(|(key, sketch)| (*key, sketch)))?;
        completed = completed.saturating_add(batch_len);
    }
    if let Some(reporter) = &reporter {
        reporter.emit("dedup_index", completed, None, true, None, None)?;
    }
    match args.emit {
        DedupOutput::Pairs => write_pairs(args, check.scored_pairs(index.candidate_pairs()))?,
        DedupOutput::Groups => {
            let groups = index
                .duplicate_groups_with(args.min_size, |left, right| check.accepts(left, right));
            write_groups(args, &groups)?;
        }
    }
    if let Some(reporter) = &reporter {
        reporter.emit("dedup_output", completed, Some(completed), true, None, None)?;
    }
    Ok(())
}

fn dedup64(args: &DedupArgs) -> Result<(), Box<dyn Error>> {
    let reporter = ProgressReporter::new(&args.progress)?;
    let mut index = LshIndex64::new(args.threshold, args.num_perm, args.seed)?;
    let mut check = SimilarityCheck::new(args.min_similarity);
    let reader = open_reader(&args.input)?;
    let mut batch = Vec::with_capacity(args.batch_size);
    let mut completed = 0_usize;
    for_json_lines(reader, |_line, record: Record| {
        let sketch = make_sketch64(
            record.values.as_deref(),
            record.signature.as_deref(),
            record.scheme.as_deref(),
            record.seed,
            args.num_perm,
            args.seed,
        )?;
        check.remember(record.key, sketch.signature());
        batch.push((record.key, sketch));
        if batch.len() == args.batch_size {
            let batch_len = batch.len();
            index.insert_many(batch.iter().map(|(key, sketch)| (*key, sketch)))?;
            batch.clear();
            completed = completed.saturating_add(batch_len);
            if let Some(reporter) = &reporter {
                reporter.emit("dedup_index", completed, None, false, None, None)?;
            }
        }
        Ok(())
    })?;
    if !batch.is_empty() {
        let batch_len = batch.len();
        index.insert_many(batch.iter().map(|(key, sketch)| (*key, sketch)))?;
        completed = completed.saturating_add(batch_len);
    }
    if let Some(reporter) = &reporter {
        reporter.emit("dedup_index", completed, None, true, None, None)?;
    }
    match args.emit {
        DedupOutput::Pairs => write_pairs(args, check.scored_pairs(index.candidate_pairs()))?,
        DedupOutput::Groups => {
            let groups = index
                .duplicate_groups_with(args.min_size, |left, right| check.accepts(left, right));
            write_groups(args, &groups)?;
        }
    }
    if let Some(reporter) = &reporter {
        reporter.emit("dedup_output", completed, Some(completed), true, None, None)?;
    }
    Ok(())
}

fn write_pairs(
    args: &DedupArgs,
    pairs: impl IntoIterator<Item = (u64, u64, Option<f64>)>,
) -> Result<(), Box<dyn Error>> {
    let mut writer = open_writer(&args.output, &args.input)?;
    for (left, right, similarity) in pairs {
        if args.json {
            serde_json::to_writer(
                &mut writer,
                &PairResult {
                    left,
                    right,
                    similarity,
                },
            )?;
            writeln!(writer)?;
        } else if let Some(similarity) = similarity {
            writeln!(writer, "{left}\t{right}\t{similarity:.4}")?;
        } else {
            writeln!(writer, "{left}\t{right}")?;
        }
    }
    writer.flush()?;
    Ok(())
}

fn write_groups(args: &DedupArgs, groups: &[DuplicateGroup]) -> Result<(), Box<dyn Error>> {
    let mut writer = open_writer(&args.output, &args.input)?;
    for group in groups {
        if args.json {
            serde_json::to_writer(
                &mut writer,
                &GroupResult {
                    representative: group.representative(),
                    members: group.members(),
                },
            )?;
            writeln!(writer)?;
        } else {
            writeln!(
                writer,
                "{}\t{}",
                group.representative(),
                join_u64(group.members())
            )?;
        }
    }
    writer.flush()?;
    Ok(())
}

fn stats(args: &StatsArgs) -> Result<(), Box<dyn Error>> {
    match index_signature_scheme(&args.index)? {
        SignatureScheme::PariAffine32V1 => {
            let store = PersistentIndex32::open(&args.index)?;
            print_stats(
                store.stats()?,
                store.num_perm(),
                store.seed(),
                store.threshold(),
                args.json,
            )?;
        }
        SignatureScheme::PariAffine64V1 => {
            let store = PersistentIndex64::open(&args.index)?;
            print_stats(
                store.stats()?,
                store.num_perm(),
                store.seed(),
                store.threshold(),
                args.json,
            )?;
        }
    }
    Ok(())
}

fn print_stats(
    current: StoreStats,
    num_perm: usize,
    seed: u64,
    threshold: f64,
    json: bool,
) -> Result<(), Box<dyn Error>> {
    if json {
        outln!(
            "{}",
            serde_json::json!({
                "items": current.items,
                "file_bytes": current.file_bytes,
                "dirty": current.dirty,
                "bands": current.bands,
                "rows": current.rows,
                "committed_buckets": current.committed_buckets,
                "overlay_buckets": current.overlay_buckets,
                "suppressed_base_keys": current.suppressed_base_keys,
                "delta_generations": current.delta_generations,
                "delta_bytes": current.delta_bytes,
                "committed_bucket_distribution": bucket_distribution_json(current.committed_distribution),
                "overlay_bucket_distribution": bucket_distribution_json(current.overlay_distribution),
                "query_metrics": current.queries.map(query_metrics_json),
                "num_perm": num_perm,
                "seed": seed,
                "threshold": threshold,
                "stores_signatures": current.stores_signatures,
            })
        );
    } else {
        outln!("items: {}", current.items);
        outln!("file bytes: {}", current.file_bytes);
        outln!("threshold: {threshold}");
        outln!("num perm: {num_perm}");
        outln!("seed: {seed}");
        outln!("bands: {}", current.bands);
        outln!("rows: {}", current.rows);
        outln!("committed buckets: {}", current.committed_buckets);
        outln!("overlay buckets: {}", current.overlay_buckets);
        outln!("suppressed base keys: {}", current.suppressed_base_keys);
        outln!(
            "delta generations: {} ({} bytes)",
            current.delta_generations,
            current.delta_bytes
        );
        outln!(
            "committed bucket members: {} total, min {}, p50 {}, p95 {}, p99 {}, max {}, average {:.3}",
            current.committed_distribution.memberships,
            current.committed_distribution.minimum,
            current.committed_distribution.p50,
            current.committed_distribution.p95,
            current.committed_distribution.p99,
            current.committed_distribution.maximum,
            current.committed_distribution.average_members(),
        );
        outln!(
            "overlay bucket members: {} total, min {}, p50 {}, p95 {}, p99 {}, max {}, average {:.3}",
            current.overlay_distribution.memberships,
            current.overlay_distribution.minimum,
            current.overlay_distribution.p50,
            current.overlay_distribution.p95,
            current.overlay_distribution.p99,
            current.overlay_distribution.maximum,
            current.overlay_distribution.average_members(),
        );
        outln!("dirty: {}", current.dirty);
        outln!("stores signatures: {}", current.stores_signatures);
    }
    Ok(())
}

fn compact(args: &CompactArgs) -> Result<(), Box<dyn Error>> {
    match index_signature_scheme(&args.index)? {
        SignatureScheme::PariAffine32V1 => PersistentIndex32::open(&args.index)?.compact()?,
        SignatureScheme::PariAffine64V1 => PersistentIndex64::open(&args.index)?.compact()?,
    }
    Ok(())
}

#[derive(Debug, Serialize)]
struct BuildSummary {
    records: u64,
    buckets: u64,
    output_bytes: u64,
    spill_runs: usize,
    reused_source_buckets: bool,
    folded_generations: usize,
}

fn build_read_only(args: &BuildReadOnlyArgs) -> Result<(), Box<dyn Error>> {
    let stats = build_external(
        &args.index,
        &args.output,
        BuildOptions {
            max_buffer_records: args.max_buffer_records,
            reuse_source_buckets: !args.no_reuse_buckets,
        },
    )?;
    let summary = BuildSummary {
        records: stats.records,
        buckets: stats.buckets,
        output_bytes: stats.output_bytes,
        spill_runs: stats.spill_runs,
        reused_source_buckets: stats.reused_source_buckets,
        folded_generations: stats.folded_generations,
    };
    if args.json {
        outln!("{}", serde_json::to_string(&summary)?);
    } else {
        let folded = match summary.folded_generations {
            0 => String::new(),
            1 => ", folded 1 appended commit".to_owned(),
            count => format!(", folded {count} appended commits"),
        };
        outln!(
            "built {} ({} bytes, {} buckets, {} records, {}{folded})",
            args.output.display(),
            summary.output_bytes,
            summary.buckets,
            summary.records,
            if summary.reused_source_buckets {
                "reused source buckets".to_owned()
            } else {
                format!("rebuilt buckets from {} sorted runs", summary.spill_runs)
            }
        );
    }
    Ok(())
}

fn verify(args: &VerifyArgs) -> Result<(), Box<dyn Error>> {
    let reporter = ProgressReporter::new(&args.progress)?;
    let mut file = File::open(&args.index)?;
    let layout = FileLayout::read_with_generations(&mut file)?;
    // Opening applies every delta generation and checks its key changes.
    match layout.metadata().signature_scheme() {
        SignatureScheme::PariAffine32V1 => {
            PersistentIndex32::open(&args.index)?;
        }
        SignatureScheme::PariAffine64V1 => {
            PersistentIndex64::open(&args.index)?;
        }
    }
    let bands = usize::try_from(layout.metadata().bands())?;
    // The base snapshot, then each delta generation, in commit order.
    let generations: Vec<&[pari_format::SectionDescriptor]> = std::iter::once(layout.sections())
        .chain(
            layout
                .deltas()
                .iter()
                .map(pari_format::DeltaLayout::sections),
        )
        .collect();
    let mut sections = 0_usize;
    let mut bucket_descriptors = Vec::with_capacity(generations.len());
    for generation in &generations {
        let mut descriptors = Vec::new();
        for descriptor in generation.iter().copied() {
            let _ = layout.read_section(&mut file, descriptor)?;
            if descriptor.kind() == SectionKind::Buckets {
                descriptors.push(descriptor);
            }
        }
        sections += generation.len();
        bucket_descriptors.push(descriptors);
    }
    let bucket_sections = bucket_descriptors.iter().map(Vec::len).sum();
    // Validates every directory and each generation's key order without
    // retaining one location per bucket; members are then checked a section
    // at a time.
    let mut total = 0_usize;
    for descriptors in &bucket_descriptors {
        total +=
            usize::try_from(BucketDirectory::read(&layout, &mut file, descriptors, bands)?.len())?;
    }
    let mut members_checked = 0_u64;
    let mut completed = 0_usize;
    for descriptor in bucket_descriptors.into_iter().flatten() {
        for location in decode_bucket_segment(&layout, &mut file, descriptor, bands)? {
            let members = read_bucket_members(&layout, &mut file, location)?;
            members_checked = members_checked
                .checked_add(u64::try_from(members.len())?)
                .ok_or("member count overflow")?;
            completed += 1;
            if let Some(reporter) = &reporter {
                if reporter.is_due(completed) {
                    reporter.emit(
                        "verify",
                        completed,
                        Some(total),
                        false,
                        Some(members_checked),
                        None,
                    )?;
                }
            }
        }
    }
    if completed != total {
        return Err("bucket directory count changed while verifying".into());
    }
    if let Some(reporter) = &reporter {
        reporter.emit(
            "verify",
            total,
            Some(total),
            true,
            Some(members_checked),
            None,
        )?;
    }
    let result = VerifyResult {
        valid: true,
        sections,
        bucket_sections,
        buckets: total,
        members_checked,
        delta_generations: layout.deltas().len(),
    };
    if args.json {
        outln!("{}", serde_json::to_string(&result)?);
    } else {
        outln!(
            "valid: {} sections, {} bucket sections, {} buckets, {} members checked, {} delta generations",
            result.sections,
            result.bucket_sections,
            result.buckets,
            result.members_checked,
            result.delta_generations
        );
    }
    Ok(())
}

fn make_sketch32(
    values: Option<&[String]>,
    signature: Option<&[u64]>,
    scheme: Option<&str>,
    signature_seed: Option<u64>,
    num_perm: usize,
    seed: u64,
) -> Result<MinHash32, Box<dyn Error>> {
    match (values, signature) {
        (Some(values), None) => {
            if scheme.is_some() {
                return Err("scheme is only valid with a precomputed signature".into());
            }
            if signature_seed.is_some() {
                return Err("seed is only valid with a precomputed signature".into());
            }
            let mut sketch = MinHash32::new(num_perm, seed)?;
            for value in values {
                sketch.update(value.as_bytes());
            }
            Ok(sketch)
        }
        (None, Some(signature)) => {
            let scheme = scheme.ok_or("precomputed signatures require a scheme field")?;
            if scheme != AFFINE32_SCHEME {
                return Err(format!(
                    "unsupported signature scheme {scheme:?}; expected {AFFINE32_SCHEME:?}"
                )
                .into());
            }
            if signature.len() != num_perm {
                return Err(format!(
                    "precomputed signature has {} values; expected {num_perm}",
                    signature.len()
                )
                .into());
            }
            validate_signature_seed(signature_seed, seed)?;
            let signature = signature
                .iter()
                .copied()
                .enumerate()
                .map(|(index, value)| {
                    u32::try_from(value).map_err(|_| {
                        format!(
                            "precomputed affine32 signature value {index} ({value}) exceeds {}",
                            u32::MAX
                        )
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(MinHash32::from_signature(signature, seed)?)
        }
        (Some(_), Some(_)) => {
            Err("record must contain either values or signature, not both".into())
        }
        (None, None) => Err("record must contain either values or signature".into()),
    }
}

fn make_sketch64(
    values: Option<&[String]>,
    signature: Option<&[u64]>,
    scheme: Option<&str>,
    signature_seed: Option<u64>,
    num_perm: usize,
    seed: u64,
) -> Result<MinHash64, Box<dyn Error>> {
    match (values, signature) {
        (Some(values), None) => {
            if scheme.is_some() {
                return Err("scheme is only valid with a precomputed signature".into());
            }
            if signature_seed.is_some() {
                return Err("seed is only valid with a precomputed signature".into());
            }
            let mut sketch = MinHash64::new(num_perm, seed)?;
            for value in values {
                sketch.update(value.as_bytes());
            }
            Ok(sketch)
        }
        (None, Some(signature)) => {
            let scheme = scheme.ok_or("precomputed signatures require a scheme field")?;
            if scheme != AFFINE64_SCHEME {
                return Err(format!(
                    "incompatible signature scheme {scheme:?}; expected {AFFINE64_SCHEME:?}"
                )
                .into());
            }
            if signature.len() != num_perm {
                return Err(format!(
                    "precomputed signature has {} values; expected {num_perm}",
                    signature.len()
                )
                .into());
            }
            validate_signature_seed(signature_seed, seed)?;
            Ok(MinHash64::from_signature(signature.to_vec(), seed)?)
        }
        (Some(_), Some(_)) => {
            Err("record must contain either values or signature, not both".into())
        }
        (None, None) => Err("record must contain either values or signature".into()),
    }
}

fn validate_signature_seed(actual: Option<u64>, expected: u64) -> Result<(), Box<dyn Error>> {
    if let Some(actual) = actual {
        if actual != expected {
            return Err(format!(
                "precomputed signature seed {actual} does not match configured seed {expected}"
            )
            .into());
        }
    }
    Ok(())
}

fn index_signature_scheme(path: &Path) -> Result<SignatureScheme, Box<dyn Error>> {
    let mut file = File::open(path)?;
    let layout = FileLayout::read_with_generations(&mut file)?;
    Ok(layout.metadata().signature_scheme())
}

fn for_json_lines<T, F>(
    mut reader: Box<dyn BufRead>,
    mut operation: F,
) -> Result<(), Box<dyn Error>>
where
    T: for<'de> Deserialize<'de>,
    F: FnMut(usize, T) -> Result<(), Box<dyn Error>>,
{
    let mut line = String::new();
    let mut line_number = 0_usize;
    loop {
        line.clear();
        let bytes = reader.read_line(&mut line)?;
        if bytes == 0 {
            break;
        }
        line_number += 1;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value = serde_json::from_str::<T>(trimmed)
            .map_err(|error| format!("invalid JSON on line {line_number}: {error}"))?;
        operation(line_number, value).map_err(|error| -> Box<dyn Error> {
            if is_broken_pipe(error.as_ref()) {
                error
            } else {
                format!("line {line_number}: {error}").into()
            }
        })?;
    }
    Ok(())
}

fn is_broken_pipe(error: &(dyn Error + 'static)) -> bool {
    let mut current = Some(error);
    while let Some(error) = current {
        if error
            .downcast_ref::<io::Error>()
            .is_some_and(|error| error.kind() == io::ErrorKind::BrokenPipe)
        {
            return true;
        }
        current = error.source();
    }
    false
}

fn open_reader(path: &str) -> Result<Box<dyn BufRead>, Box<dyn Error>> {
    if path == "-" {
        Ok(Box::new(BufReader::new(io::stdin())))
    } else {
        Ok(Box::new(BufReader::new(File::open(path)?)))
    }
}

fn ensure_distinct_input_output(input: &str, output: &str) -> Result<(), Box<dyn Error>> {
    if input == "-" || output == "-" {
        return Ok(());
    }
    match is_same_file(input, output) {
        Ok(true) => Err("dedup input and output must refer to different files".into()),
        Ok(false) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        // An output path that cannot be resolved (for example, one beneath a
        // regular file) cannot alias the input; creating it reports the error.
        Err(_) if fs::metadata(output).is_err() => Ok(()),
        Err(error) => Err(io::Error::new(
            error.kind(),
            format!("failed to compare dedup input {input} with output {output}: {error}"),
        )
        .into()),
    }
}

fn open_writer(path: &str, input: &str) -> Result<Box<dyn Write>, Box<dyn Error>> {
    ensure_distinct_input_output(input, path)?;
    if path == "-" {
        Ok(Box::new(BufWriter::new(io::stdout())))
    } else {
        let path = Path::new(path);
        let context = |error: io::Error| {
            io::Error::new(
                error.kind(),
                format!("failed to create output {}: {error}", path.display()),
            )
        };
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent).map_err(context)?;
        }
        Ok(Box::new(BufWriter::new(
            File::create(path).map_err(context)?,
        )))
    }
}

fn join_u64(values: &[u64]) -> String {
    values
        .iter()
        .map(u64::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

fn bucket_distribution_json(distribution: BucketDistribution) -> serde_json::Value {
    serde_json::json!({
        "exact": true,
        "buckets": distribution.buckets,
        "memberships": distribution.memberships,
        "minimum": distribution.minimum,
        "p50": distribution.p50,
        "p95": distribution.p95,
        "p99": distribution.p99,
        "maximum": distribution.maximum,
        "average": distribution.average_members(),
    })
}

fn query_metrics_json(metrics: QueryMetrics) -> serde_json::Value {
    serde_json::json!({
        "scope": "process_local",
        "counts_exact": true,
        "latency_observed": true,
        "operations": metrics.operations,
        "queries": metrics.queries,
        "candidates": metrics.candidates,
        "possible_candidates": metrics.possible_candidates,
        "candidate_rate": metrics.candidate_rate(),
        "total_latency_ns": metrics.total_latency_ns,
        "max_latency_ns": metrics.max_latency_ns,
        "average_operation_ms": metrics.average_operation_ms(),
    })
}

#[allow(clippy::cast_precision_loss)]
fn u64_to_f64(value: u64) -> f64 {
    value as f64
}

fn ratio(numerator: u64, denominator: usize) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        u64_to_f64(numerator) / u64_to_f64(u64::try_from(denominator).unwrap_or(u64::MAX))
    }
}

#[cfg(test)]
mod index_output_transaction_tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::{sync_parent_directory, IndexOutputTransaction};
    use std::{fs, io, path::PathBuf};

    static NEXT_PATH: AtomicU64 = AtomicU64::new(0);

    fn output_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "pari-cli-transaction-{name}-{}-{}.pari",
            std::process::id(),
            NEXT_PATH.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn staged_transaction(name: &str) -> (IndexOutputTransaction, PathBuf, PathBuf) {
        let final_path = output_path(name);
        let _ = fs::remove_file(&final_path);
        let transaction = IndexOutputTransaction::begin(&final_path).expect("transaction");
        let staged_path = transaction.staged_path().to_path_buf();
        fs::write(&staged_path, b"complete index bytes").expect("staged index");
        (transaction, final_path, staged_path)
    }

    #[test]
    fn cleanup_failure_after_link_rolls_back_owned_final() {
        let (transaction, final_path, staged_path) = staged_transaction("cleanup-failure");

        let error = transaction
            .publish_with(
                |_| Err(io::Error::other("forced staged cleanup failure")),
                sync_parent_directory,
            )
            .expect_err("publication must fail");

        assert!(error.to_string().contains("forced staged cleanup failure"));
        assert!(!final_path.exists(), "owned final path was not rolled back");
        assert!(!staged_path.exists(), "staged path was not cleaned");
    }

    #[test]
    fn cleanup_failure_preserves_concurrent_final_replacement() {
        let (transaction, final_path, staged_path) = staged_transaction("replacement");
        let replacement_path = final_path.clone();
        let replacement = b"concurrent replacement";

        let error = transaction
            .publish_with(
                move |_| {
                    fs::remove_file(&replacement_path)?;
                    fs::write(&replacement_path, replacement)?;
                    Err(io::Error::other("forced staged cleanup failure"))
                },
                sync_parent_directory,
            )
            .expect_err("publication must fail");

        assert!(error.to_string().contains("forced staged cleanup failure"));
        assert_eq!(
            fs::read(&final_path).expect("replacement remains"),
            replacement
        );
        assert!(!staged_path.exists(), "staged path was not cleaned");
        let _ = fs::remove_file(final_path);
    }

    #[test]
    fn directory_sync_failure_rolls_back_owned_final() {
        let (transaction, final_path, staged_path) = staged_transaction("sync-failure");

        let error = transaction
            .publish_with(
                |path| fs::remove_file(path),
                |_| Err(io::Error::other("forced directory sync failure")),
            )
            .expect_err("publication must fail");

        assert!(error.to_string().contains("forced directory sync failure"));
        assert!(!final_path.exists(), "owned final path was not rolled back");
        assert!(!staged_path.exists(), "staged path was not removed");
    }
}
