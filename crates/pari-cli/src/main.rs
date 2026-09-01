#![forbid(unsafe_code)]

use std::{
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
use pari_core::{MinHash32, AFFINE32_SCHEME};
use pari_format::{
    decode_bucket_segment, read_bucket_members, validate_global_bucket_order, FileLayout,
    SectionKind,
};
use pari_index::{
    plan_lsh, BucketDistribution, LshIndex32, LshPlan, LshPlanOptions, QueryMetrics, StorageMode,
};
use pari_store::PersistentIndex32;
use same_file::Handle;
use serde::{Deserialize, Serialize};

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
    /// Records committed per batch. Smaller values reduce uncommitted overlay memory.
    #[arg(long, default_value_t = 10_000)]
    batch_size: usize,
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
    /// JSONL query path, or '-' for stdin.
    #[arg(short, long, default_value = "-")]
    input: String,
    /// Emit JSONL results instead of tab-separated text.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    progress: ProgressArgs,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum DedupOutput {
    Pairs,
    Groups,
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
    #[arg(long, value_enum, default_value_t = DedupOutput::Groups)]
    emit: DedupOutput,
    /// Minimum component size when emitting groups.
    #[arg(long, default_value_t = 2)]
    min_size: usize,
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
    signature: Option<Vec<u32>>,
    #[serde(default)]
    scheme: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct QueryRecord {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    values: Option<Vec<String>>,
    #[serde(default)]
    signature: Option<Vec<u32>>,
    #[serde(default)]
    scheme: Option<String>,
}

#[derive(Debug, Serialize)]
struct SearchResult<'a> {
    query: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: &'a Option<String>,
    candidates: Vec<u64>,
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
        Commands::Completion(args) => {
            let mut command = Cli::command();
            let name = command.get_name().to_owned();
            generate(args.shell, &mut command, name, &mut io::stdout());
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
    print_plan(plan_lsh(options)?, args.json);
    Ok(())
}

fn explain(args: &ExplainArgs) -> Result<(), Box<dyn Error>> {
    let store = PersistentIndex32::open(&args.index)?;
    print_plan(store.explain()?, args.json);
    Ok(())
}

fn print_plan(plan: LshPlan, json: bool) {
    if json {
        println!("{}", lsh_plan_json(plan));
        return;
    }

    println!(
        "model: {} (analytical/model-based, not a measured guarantee)",
        plan.model
    );
    println!("items: {}", plan.expected_items);
    println!("threshold: {}", plan.threshold);
    println!("num perm: {}", plan.num_perm);
    println!(
        "parameters: {} bands x {} rows ({})",
        plan.params.bands,
        plan.params.rows,
        plan.parameter_source.as_str()
    );
    println!(
        "permutations: {} used, {} unused",
        plan.used_permutations, plan.unused_permutations
    );
    println!(
        "candidate probability at threshold: {:.6}",
        plan.candidate_probability_at_threshold
    );
    println!(
        "50% candidate similarity: {:.6}",
        plan.similarity_at_50_percent_candidates
    );
    println!("false-positive area: {:.6}", plan.false_positive_area);
    println!("false-negative area: {:.6}", plan.false_negative_area);
    println!(
        "signature bytes/item: {}",
        plan.sizes.signature_bytes_per_item
    );
    println!(
        "bucket memberships/item: {}",
        plan.bucket_memberships_per_item
    );
    println!(
        "modeled index metadata bytes: {}",
        plan.sizes.index_metadata_bytes
    );
    println!(
        "modeled in-memory index bytes: {}",
        plan.sizes.in_memory_index_bytes
    );
    println!(
        "modeled persistent index bytes: {}",
        plan.sizes.persistent_index_bytes
    );
    println!(
        "modeled lazy resident bytes: {}",
        plan.sizes.lazy_resident_bytes
    );
    if let Some(budget) = plan.memory_budget_bytes {
        println!("memory budget bytes: {budget}");
        println!(
            "in-memory fits with 50% headroom: {}",
            plan.in_memory_fits_budget.unwrap_or(false)
        );
        println!(
            "persistent/lazy fits with 50% headroom: {}",
            plan.persistent_fits_budget.unwrap_or(false)
        );
    }
    println!("requested storage: {}", plan.requested_storage);
    println!("recommended storage: {}", plan.recommended_storage);
    println!("recommendation: {}", plan.recommendation_guidance());
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
        println!("{}", serde_json::to_string(&summary)?);
    } else {
        println!(
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
    let mut store = PersistentIndex32::create(output, args.threshold, args.num_perm, args.seed)?;
    let reporter = ProgressReporter::new(&args.progress)?;
    let reader = open_reader(&args.input)?;
    let mut batch = Vec::with_capacity(args.batch_size);
    let mut completed = 0_usize;
    for_json_lines(reader, |_line, record: Record| {
        batch.push((
            record.key,
            make_sketch(
                record.values.as_deref(),
                record.signature.as_deref(),
                record.scheme.as_deref(),
                args.num_perm,
                args.seed,
            )?,
        ));
        if batch.len() == args.batch_size {
            let batch_len = batch.len();
            insert_batch(&mut store, &batch)?;
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
        insert_batch(&mut store, &batch)?;
        completed = completed.saturating_add(batch_len);
    }
    store.sync()?;
    if let Some(reporter) = &reporter {
        reporter.emit("index", completed, None, true, None, None)?;
    }
    let current = store.stats()?;
    let summary = IndexSummary {
        items: current.items,
        file_bytes: current.file_bytes,
        bands: current.bands,
        rows: current.rows,
    };
    store.close()?;
    Ok(summary)
}

fn insert_batch(
    store: &mut PersistentIndex32,
    batch: &[(u64, MinHash32)],
) -> Result<(), Box<dyn Error>> {
    store.insert_many(batch.iter().map(|(key, sketch)| (*key, sketch)))?;
    Ok(())
}

fn search(args: &SearchArgs) -> Result<(), Box<dyn Error>> {
    let reporter = ProgressReporter::new(&args.progress)?;
    let mut store = PersistentIndex32::open(&args.index)?;
    store.set_observability(reporter.is_some());
    let reader = open_reader(&args.input)?;
    let mut writer = BufWriter::new(io::stdout().lock());
    let mut query_index = 0_usize;
    let mut candidate_count = 0_u64;
    for_json_lines(reader, |_line, query: QueryRecord| {
        let sketch = make_sketch(
            query.values.as_deref(),
            query.signature.as_deref(),
            query.scheme.as_deref(),
            store.num_perm(),
            store.seed(),
        )?;
        let candidates = store.query(&sketch)?;
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
                },
            )?;
            writeln!(writer)?;
        } else {
            let id = query.id.as_deref().unwrap_or("-");
            writeln!(writer, "{query_index}\t{id}\t{}", join_u64(&candidates))?;
        }
        query_index += 1;
        if let Some(reporter) = &reporter {
            if reporter.is_due(query_index) {
                let possible = query_index.saturating_mul(store.len());
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
    if let Some(reporter) = &reporter {
        let possible = query_index.saturating_mul(store.len());
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
    let reporter = ProgressReporter::new(&args.progress)?;
    let mut index = LshIndex32::new(args.threshold, args.num_perm, args.seed)?;
    let reader = open_reader(&args.input)?;
    let mut batch = Vec::with_capacity(args.batch_size);
    let mut completed = 0_usize;
    for_json_lines(reader, |_line, record: Record| {
        let sketch = make_sketch(
            record.values.as_deref(),
            record.signature.as_deref(),
            record.scheme.as_deref(),
            args.num_perm,
            args.seed,
        )?;
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
    let mut writer = open_writer(&args.output)?;
    match args.emit {
        DedupOutput::Pairs => {
            for (left, right) in index.candidate_pairs() {
                if args.json {
                    serde_json::to_writer(&mut writer, &PairResult { left, right })?;
                    writeln!(writer)?;
                } else {
                    writeln!(writer, "{left}\t{right}")?;
                }
            }
        }
        DedupOutput::Groups => {
            let groups = index.duplicate_groups_with(args.min_size, |_, _| true);
            for group in &groups {
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
        }
    }
    writer.flush()?;
    if let Some(reporter) = &reporter {
        reporter.emit("dedup_output", completed, Some(completed), true, None, None)?;
    }
    Ok(())
}

fn stats(args: &StatsArgs) -> Result<(), Box<dyn Error>> {
    let store = PersistentIndex32::open(&args.index)?;
    let current = store.stats()?;
    if args.json {
        println!(
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
                "committed_bucket_distribution": bucket_distribution_json(current.committed_distribution),
                "overlay_bucket_distribution": bucket_distribution_json(current.overlay_distribution),
                "query_metrics": current.queries.map(query_metrics_json),
                "num_perm": store.num_perm(),
                "seed": store.seed(),
                "threshold": store.threshold(),
            })
        );
    } else {
        println!("items: {}", current.items);
        println!("file bytes: {}", current.file_bytes);
        println!("threshold: {}", store.threshold());
        println!("num perm: {}", store.num_perm());
        println!("seed: {}", store.seed());
        println!("bands: {}", current.bands);
        println!("rows: {}", current.rows);
        println!("committed buckets: {}", current.committed_buckets);
        println!("overlay buckets: {}", current.overlay_buckets);
        println!("suppressed base keys: {}", current.suppressed_base_keys);
        println!(
            "committed bucket members: {} total, min {}, p50 {}, p95 {}, p99 {}, max {}, average {:.3}",
            current.committed_distribution.memberships,
            current.committed_distribution.minimum,
            current.committed_distribution.p50,
            current.committed_distribution.p95,
            current.committed_distribution.p99,
            current.committed_distribution.maximum,
            current.committed_distribution.average_members(),
        );
        println!(
            "overlay bucket members: {} total, min {}, p50 {}, p95 {}, p99 {}, max {}, average {:.3}",
            current.overlay_distribution.memberships,
            current.overlay_distribution.minimum,
            current.overlay_distribution.p50,
            current.overlay_distribution.p95,
            current.overlay_distribution.p99,
            current.overlay_distribution.maximum,
            current.overlay_distribution.average_members(),
        );
        println!("dirty: {}", current.dirty);
    }
    Ok(())
}

fn verify(args: &VerifyArgs) -> Result<(), Box<dyn Error>> {
    let reporter = ProgressReporter::new(&args.progress)?;
    let mut file = File::open(&args.index)?;
    let layout = FileLayout::read_from(&mut file)?;
    let bands = usize::try_from(layout.metadata().bands())?;
    let mut locations = Vec::new();
    let mut bucket_sections = 0_usize;
    for descriptor in layout.sections().iter().copied() {
        let _ = layout.read_section(&mut file, descriptor)?;
        if descriptor.kind() == SectionKind::Buckets {
            bucket_sections += 1;
            locations.extend(decode_bucket_segment(
                &layout, &mut file, descriptor, bands,
            )?);
        }
    }
    validate_global_bucket_order(&locations)?;
    let mut members_checked = 0_u64;
    for (index, location) in locations.iter().copied().enumerate() {
        let members = read_bucket_members(&layout, &mut file, location)?;
        members_checked = members_checked
            .checked_add(u64::try_from(members.len())?)
            .ok_or("member count overflow")?;
        if let Some(reporter) = &reporter {
            let completed = index + 1;
            if reporter.is_due(completed) {
                reporter.emit(
                    "verify",
                    completed,
                    Some(locations.len()),
                    false,
                    Some(members_checked),
                    None,
                )?;
            }
        }
    }
    if let Some(reporter) = &reporter {
        reporter.emit(
            "verify",
            locations.len(),
            Some(locations.len()),
            true,
            Some(members_checked),
            None,
        )?;
    }
    let result = VerifyResult {
        valid: true,
        sections: layout.sections().len(),
        bucket_sections,
        buckets: locations.len(),
        members_checked,
    };
    if args.json {
        println!("{}", serde_json::to_string(&result)?);
    } else {
        println!(
            "valid: {} sections, {} bucket sections, {} buckets, {} members checked",
            result.sections, result.bucket_sections, result.buckets, result.members_checked
        );
    }
    Ok(())
}

fn make_sketch(
    values: Option<&[String]>,
    signature: Option<&[u32]>,
    scheme: Option<&str>,
    num_perm: usize,
    seed: u64,
) -> Result<MinHash32, Box<dyn Error>> {
    match (values, signature) {
        (Some(values), None) => {
            if scheme.is_some() {
                return Err("scheme is only valid with a precomputed signature".into());
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
            Ok(MinHash32::from_signature(signature.to_vec(), seed)?)
        }
        (Some(_), Some(_)) => {
            Err("record must contain either values or signature, not both".into())
        }
        (None, None) => Err("record must contain either values or signature".into()),
    }
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
        operation(line_number, value).map_err(|error| format!("line {line_number}: {error}"))?;
    }
    Ok(())
}

fn open_reader(path: &str) -> Result<Box<dyn BufRead>, Box<dyn Error>> {
    if path == "-" {
        Ok(Box::new(BufReader::new(io::stdin())))
    } else {
        Ok(Box::new(BufReader::new(File::open(path)?)))
    }
}

fn open_writer(path: &str) -> Result<Box<dyn Write>, Box<dyn Error>> {
    if path == "-" {
        Ok(Box::new(BufWriter::new(io::stdout())))
    } else {
        Ok(Box::new(BufWriter::new(File::create(path)?)))
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
