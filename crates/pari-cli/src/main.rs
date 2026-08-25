#![forbid(unsafe_code)]

use std::{
    error::Error,
    fs::File,
    io::{self, BufRead, BufReader, BufWriter, Write},
    path::PathBuf,
    process::ExitCode,
};

use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use clap_complete::{generate, Shell};
use pari_core::{MinHash32, AFFINE32_SCHEME};
use pari_format::{
    decode_bucket_segment, read_bucket_members, validate_global_bucket_order, FileLayout,
    SectionKind,
};
use pari_index::LshIndex32;
use pari_store::PersistentIndex32;
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

fn index(args: &IndexArgs) -> Result<(), Box<dyn Error>> {
    if args.batch_size == 0 {
        return Err("--batch-size must be positive".into());
    }
    let mut store =
        PersistentIndex32::create(&args.output, args.threshold, args.num_perm, args.seed)?;
    let reader = open_reader(&args.input)?;
    let mut batch = Vec::with_capacity(args.batch_size);
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
            insert_batch(&mut store, &batch)?;
            store.sync()?;
            batch.clear();
        }
        Ok(())
    })?;
    if !batch.is_empty() {
        insert_batch(&mut store, &batch)?;
    }
    store.sync()?;
    let current = store.stats()?;
    let summary = IndexSummary {
        items: current.items,
        file_bytes: current.file_bytes,
        bands: current.bands,
        rows: current.rows,
    };
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

fn insert_batch(
    store: &mut PersistentIndex32,
    batch: &[(u64, MinHash32)],
) -> Result<(), Box<dyn Error>> {
    store.insert_many(batch.iter().map(|(key, sketch)| (*key, sketch)))?;
    Ok(())
}

fn search(args: &SearchArgs) -> Result<(), Box<dyn Error>> {
    let store = PersistentIndex32::open(&args.index)?;
    let reader = open_reader(&args.input)?;
    let mut writer = BufWriter::new(io::stdout().lock());
    let mut query_index = 0_usize;
    for_json_lines(reader, |_line, query: QueryRecord| {
        let sketch = make_sketch(
            query.values.as_deref(),
            query.signature.as_deref(),
            query.scheme.as_deref(),
            store.num_perm(),
            store.seed(),
        )?;
        let candidates = store.query(&sketch)?;
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
        Ok(())
    })?;
    writer.flush()?;
    Ok(())
}

fn dedup(args: &DedupArgs) -> Result<(), Box<dyn Error>> {
    if args.min_size == 0 {
        return Err("--min-size must be positive".into());
    }
    let mut index = LshIndex32::new(args.threshold, args.num_perm, args.seed)?;
    let reader = open_reader(&args.input)?;
    for_json_lines(reader, |_line, record: Record| {
        let sketch = make_sketch(
            record.values.as_deref(),
            record.signature.as_deref(),
            record.scheme.as_deref(),
            args.num_perm,
            args.seed,
        )?;
        index.insert(record.key, &sketch)?;
        Ok(())
    })?;
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
        println!("dirty: {}", current.dirty);
    }
    Ok(())
}

fn verify(args: &VerifyArgs) -> Result<(), Box<dyn Error>> {
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
    for location in locations.iter().copied() {
        let members = read_bucket_members(&layout, &mut file, location)?;
        members_checked = members_checked
            .checked_add(u64::try_from(members.len())?)
            .ok_or("member count overflow")?;
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
