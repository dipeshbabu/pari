#![forbid(unsafe_code)]
//! Bounded-memory construction for Pari's paged lazy index format.
//!
//! The builder consumes a committed phase-1 `pari-store` snapshot, spills
//! fixed-width `(band, hash, key)` records into bounded sorted runs, merges the
//! runs, and streams the final v1 container atomically. Total bucket membership
//! is never materialized in memory.

use std::{
    cmp::Reverse,
    collections::BinaryHeap,
    error::Error,
    fmt,
    fs::{self, File, OpenOptions},
    io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use crc32fast::{hash as crc32, Hasher};
use pari_format::{
    Algorithm, CodecId, FileLayout, FormatError, IndexMetadata, LayoutError, SectionDescriptor,
    SectionKind, SignatureScheme,
};

const FILE_MAGIC: [u8; 8] = *b"PARIIDX\0";
const FORMAT_VERSION: u16 = 1;
const HEADER_BYTES: usize = 72;
const HEADER_BYTES_U16: u16 = 72;
const SECTION_HEADER_BYTES: usize = 16;
const SECTION_REQUIRED: u16 = 1;
const BUCKET_MAGIC: [u8; 8] = *b"PARIBKT1";
const BUCKET_HEADER_BYTES: usize = 16;
const BUCKET_HEADER_BYTES_U64: u64 = 16;
const BUCKET_RECORD_BYTES: usize = 40;
const BUCKET_RECORD_BYTES_U64: u64 = 40;
const SPILL_RECORD_BYTES: usize = 20;
const U64_BYTES: usize = 8;
const U64_BYTES_U64: u64 = 8;
const COPY_BUFFER_BYTES: usize = 64 * 1024;
const COPY_BUFFER_BYTES_U64: u64 = 64 * 1024;

/// Configuration for the bounded external builder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuildOptions {
    /// Maximum number of `(band, hash, key)` records held before a sorted spill.
    pub max_buffer_records: usize,
}

impl Default for BuildOptions {
    fn default() -> Self {
        Self {
            max_buffer_records: 262_144,
        }
    }
}

/// Measured construction state useful for memory-bound tests and benchmarks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuildStats {
    /// Total fixed-width records emitted from the source band hashes.
    pub records: u64,
    /// Number of sorted spill runs created.
    pub spill_runs: usize,
    /// Maximum number of records simultaneously held in the spill buffer.
    pub peak_buffered_records: usize,
    /// Number of distinct output buckets.
    pub buckets: u64,
    /// Final committed file size.
    pub output_bytes: u64,
}

/// Errors returned by bounded lazy-index construction.
#[derive(Debug)]
pub enum BuildError {
    /// Filesystem I/O failed.
    Io(io::Error),
    /// The source layout reader rejected the v1 container.
    Layout(LayoutError),
    /// Source or output metadata violates the stable format.
    Format(FormatError),
    /// The source snapshot violates the phase-1 storage contract.
    InvalidSource { reason: &'static str },
    /// Builder configuration is invalid.
    InvalidOptions { reason: &'static str },
    /// The destination already exists.
    AlreadyExists(PathBuf),
    /// Checked arithmetic or platform conversion overflowed.
    LengthOverflow,
}

impl fmt::Display for BuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "bounded Pari builder I/O failed: {error}"),
            Self::Layout(error) => error.fmt(formatter),
            Self::Format(error) => error.fmt(formatter),
            Self::InvalidSource { reason } => write!(formatter, "invalid phase-1 source: {reason}"),
            Self::InvalidOptions { reason } => write!(formatter, "invalid build options: {reason}"),
            Self::AlreadyExists(path) => write!(
                formatter,
                "lazy index destination already exists: {}",
                path.display()
            ),
            Self::LengthOverflow => {
                formatter.write_str("bounded builder length arithmetic overflowed")
            }
        }
    }
}

impl Error for BuildError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Layout(error) => Some(error),
            Self::Format(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for BuildError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<LayoutError> for BuildError {
    fn from(error: LayoutError) -> Self {
        Self::Layout(error)
    }
}

impl From<FormatError> for BuildError {
    fn from(error: FormatError) -> Self {
        Self::Format(error)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct SpillRecord {
    band: u32,
    hash: u64,
    key: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct HeapEntry {
    record: SpillRecord,
    run: usize,
}

#[derive(Debug, Clone, Copy)]
struct DescriptorDraft {
    band: u32,
    member_count: u32,
    hash: u64,
    relative_offset: u64,
    member_bytes: u64,
    checksum: u32,
}

#[derive(Debug)]
struct TemporaryFiles {
    paths: Vec<PathBuf>,
}

impl TemporaryFiles {
    fn new() -> Self {
        Self { paths: Vec::new() }
    }

    fn track(&mut self, path: PathBuf) -> PathBuf {
        self.paths.push(path.clone());
        path
    }
}

impl Drop for TemporaryFiles {
    fn drop(&mut self) {
        for path in &self.paths {
            let _ = fs::remove_file(path);
        }
    }
}

/// Build a lazy paged index with bounded spill-buffer memory.
///
/// The source must be a committed phase-1 snapshot containing `Keys` and
/// `BandHashes`. The destination is created atomically and is never replaced if
/// it already exists.
pub fn build_external(
    source: impl AsRef<Path>,
    destination: impl AsRef<Path>,
    options: BuildOptions,
) -> Result<BuildStats, BuildError> {
    if options.max_buffer_records == 0 {
        return Err(BuildError::InvalidOptions {
            reason: "max_buffer_records must be positive",
        });
    }
    let source = source.as_ref();
    let destination = destination.as_ref();
    if destination.exists() {
        return Err(BuildError::AlreadyExists(destination.to_path_buf()));
    }

    let mut source_file = File::open(source)?;
    let layout = FileLayout::read_from(&mut source_file)?;
    validate_source_metadata(layout.metadata())?;
    let keys = required_section(&layout, SectionKind::Keys, "missing keys section")?;
    let band_hashes = required_section(
        &layout,
        SectionKind::BandHashes,
        "missing band-hash section",
    )?;
    verify_section_streaming(&mut source_file, keys)?;
    verify_section_streaming(&mut source_file, band_hashes)?;

    let item_count = read_record_count(&mut source_file, keys)?;
    let hash_count = read_record_count(&mut source_file, band_hashes)?;
    if item_count != hash_count {
        return Err(BuildError::InvalidSource {
            reason: "key and band-hash record counts differ",
        });
    }
    let bands =
        usize::try_from(layout.metadata().bands()).map_err(|_| BuildError::LengthOverflow)?;
    validate_source_lengths(keys, band_hashes, item_count, bands)?;

    let nonce = build_nonce()?;
    let mut temporary = TemporaryFiles::new();
    let run_paths = spill_runs(
        &mut source_file,
        keys,
        band_hashes,
        item_count,
        bands,
        destination,
        nonce,
        options,
        &mut temporary,
    )?;
    let records = u64::try_from(item_count)
        .map_err(|_| BuildError::LengthOverflow)?
        .checked_mul(u64::try_from(bands).map_err(|_| BuildError::LengthOverflow)?)
        .ok_or(BuildError::LengthOverflow)?;
    let peak_buffered_records = options
        .max_buffer_records
        .min(usize::try_from(records).unwrap_or(usize::MAX));

    let descriptors = temporary.track(temp_path(destination, nonce, "descriptors"));
    let memberships = temporary.track(temp_path(destination, nonce, "memberships"));
    let buckets = merge_runs(&run_paths, &descriptors, &memberships)?;

    let bucket_payload = temporary.track(temp_path(destination, nonce, "bucket-payload"));
    let (bucket_bytes, bucket_checksum) =
        assemble_bucket_payload(&descriptors, &memberships, buckets, &bucket_payload)?;

    let destination_temp = temporary.track(temp_path(destination, nonce, "commit"));
    write_final_container(
        &mut source_file,
        &layout,
        keys,
        &bucket_payload,
        bucket_bytes,
        bucket_checksum,
        &destination_temp,
    )?;
    fs::rename(&destination_temp, destination)?;
    sync_parent(destination)?;
    let output_bytes = fs::metadata(destination)?.len();

    Ok(BuildStats {
        records,
        spill_runs: run_paths.len(),
        peak_buffered_records,
        buckets,
        output_bytes,
    })
}

fn validate_source_metadata(metadata: &IndexMetadata) -> Result<(), BuildError> {
    if metadata.algorithm() != Algorithm::MinHashLsh {
        return Err(BuildError::InvalidSource {
            reason: "algorithm is not MinHash LSH",
        });
    }
    if metadata.signature_scheme() != SignatureScheme::PariAffine32V1 {
        return Err(BuildError::InvalidSource {
            reason: "signature scheme is not pari-affine32-v1",
        });
    }
    if metadata.key_codec() != CodecId::U64 {
        return Err(BuildError::InvalidSource {
            reason: "bounded builder currently requires u64 keys",
        });
    }
    if metadata.feature_flags() != 0 {
        return Err(BuildError::InvalidSource {
            reason: "bounded builder does not support feature flags",
        });
    }
    Ok(())
}

fn required_section(
    layout: &FileLayout,
    kind: SectionKind,
    reason: &'static str,
) -> Result<SectionDescriptor, BuildError> {
    layout
        .section(kind)
        .ok_or(BuildError::InvalidSource { reason })
}

fn validate_source_lengths(
    keys: SectionDescriptor,
    hashes: SectionDescriptor,
    item_count: usize,
    bands: usize,
) -> Result<(), BuildError> {
    let keys_expected = U64_BYTES
        .checked_add(
            item_count
                .checked_mul(U64_BYTES)
                .ok_or(BuildError::LengthOverflow)?,
        )
        .ok_or(BuildError::LengthOverflow)?;
    let row_bytes = bands
        .checked_mul(U64_BYTES)
        .ok_or(BuildError::LengthOverflow)?;
    let hashes_expected = U64_BYTES
        .checked_add(
            item_count
                .checked_mul(row_bytes)
                .ok_or(BuildError::LengthOverflow)?,
        )
        .ok_or(BuildError::LengthOverflow)?;
    if keys.payload_length()
        != u64::try_from(keys_expected).map_err(|_| BuildError::LengthOverflow)?
    {
        return Err(BuildError::InvalidSource {
            reason: "keys section length disagrees with its record count",
        });
    }
    if hashes.payload_length()
        != u64::try_from(hashes_expected).map_err(|_| BuildError::LengthOverflow)?
    {
        return Err(BuildError::InvalidSource {
            reason: "band-hash section length disagrees with its record count",
        });
    }
    Ok(())
}

fn verify_section_streaming(
    source: &mut File,
    descriptor: SectionDescriptor,
) -> Result<(), BuildError> {
    source.seek(SeekFrom::Start(descriptor.payload_offset()))?;
    let mut remaining = descriptor.payload_length();
    let mut buffer = vec![0_u8; COPY_BUFFER_BYTES];
    let mut hasher = Hasher::new();
    while remaining > 0 {
        let chunk = usize::try_from(remaining.min(COPY_BUFFER_BYTES_U64))
            .map_err(|_| BuildError::LengthOverflow)?;
        source.read_exact(&mut buffer[..chunk])?;
        hasher.update(&buffer[..chunk]);
        remaining -= u64::try_from(chunk).map_err(|_| BuildError::LengthOverflow)?;
    }
    let actual = hasher.finalize();
    if actual != descriptor.checksum() {
        return Err(FormatError::SectionChecksumMismatch {
            expected: descriptor.checksum(),
            actual,
        }
        .into());
    }
    Ok(())
}

fn read_record_count(
    source: &mut File,
    descriptor: SectionDescriptor,
) -> Result<usize, BuildError> {
    let mut raw = [0_u8; U64_BYTES];
    source.seek(SeekFrom::Start(descriptor.payload_offset()))?;
    source.read_exact(&mut raw)?;
    usize::try_from(u64::from_le_bytes(raw)).map_err(|_| BuildError::LengthOverflow)
}

#[allow(clippy::too_many_arguments)]
fn spill_runs(
    source: &mut File,
    keys: SectionDescriptor,
    hashes: SectionDescriptor,
    item_count: usize,
    bands: usize,
    destination: &Path,
    nonce: u128,
    options: BuildOptions,
    temporary: &mut TemporaryFiles,
) -> Result<Vec<PathBuf>, BuildError> {
    let row_bytes = bands
        .checked_mul(U64_BYTES)
        .ok_or(BuildError::LengthOverflow)?;
    let mut hash_row = vec![0_u8; row_bytes];
    let mut key_raw = [0_u8; U64_BYTES];
    let mut buffer = Vec::with_capacity(options.max_buffer_records);
    let mut runs = Vec::new();

    for item in 0..item_count {
        let item_offset = item
            .checked_mul(U64_BYTES)
            .and_then(|value| value.checked_add(U64_BYTES))
            .ok_or(BuildError::LengthOverflow)?;
        source.seek(SeekFrom::Start(
            keys.payload_offset()
                .checked_add(u64::try_from(item_offset).map_err(|_| BuildError::LengthOverflow)?)
                .ok_or(BuildError::LengthOverflow)?,
        ))?;
        source.read_exact(&mut key_raw)?;
        let key = u64::from_le_bytes(key_raw);

        let row_offset = item
            .checked_mul(row_bytes)
            .and_then(|value| value.checked_add(U64_BYTES))
            .ok_or(BuildError::LengthOverflow)?;
        source.seek(SeekFrom::Start(
            hashes
                .payload_offset()
                .checked_add(u64::try_from(row_offset).map_err(|_| BuildError::LengthOverflow)?)
                .ok_or(BuildError::LengthOverflow)?,
        ))?;
        source.read_exact(&mut hash_row)?;
        for (band, chunk) in hash_row.chunks_exact(U64_BYTES).enumerate() {
            let raw: [u8; U64_BYTES] = chunk.try_into().map_err(|_| BuildError::InvalidSource {
                reason: "truncated fixed-width band hash",
            })?;
            buffer.push(SpillRecord {
                band: u32::try_from(band).map_err(|_| BuildError::LengthOverflow)?,
                hash: u64::from_le_bytes(raw),
                key,
            });
            if buffer.len() == options.max_buffer_records {
                spill_one_run(
                    &mut buffer,
                    destination,
                    nonce,
                    runs.len(),
                    temporary,
                    &mut runs,
                )?;
            }
        }
    }
    if !buffer.is_empty() {
        spill_one_run(
            &mut buffer,
            destination,
            nonce,
            runs.len(),
            temporary,
            &mut runs,
        )?;
    }
    Ok(runs)
}

fn spill_one_run(
    buffer: &mut Vec<SpillRecord>,
    destination: &Path,
    nonce: u128,
    index: usize,
    temporary: &mut TemporaryFiles,
    runs: &mut Vec<PathBuf>,
) -> Result<(), BuildError> {
    buffer.sort_unstable();
    let path = temporary.track(temp_path(destination, nonce, &format!("run-{index}")));
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    let mut writer = BufWriter::new(file);
    for record in buffer.iter().copied() {
        write_spill_record(&mut writer, record)?;
    }
    writer.flush()?;
    writer.get_ref().sync_all()?;
    buffer.clear();
    runs.push(path);
    Ok(())
}

fn merge_runs(
    run_paths: &[PathBuf],
    descriptor_path: &Path,
    membership_path: &Path,
) -> Result<u64, BuildError> {
    let mut readers = run_paths
        .iter()
        .map(|path| File::open(path).map(BufReader::new))
        .collect::<Result<Vec<_>, _>>()?;
    let descriptor_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(descriptor_path)?;
    let membership_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(membership_path)?;
    let mut descriptors = BufWriter::new(descriptor_file);
    let mut memberships = BufWriter::new(membership_file);
    let mut heap = BinaryHeap::new();
    for (run, reader) in readers.iter_mut().enumerate() {
        if let Some(record) = read_spill_record(reader)? {
            heap.push(Reverse(HeapEntry { record, run }));
        }
    }

    let mut current_key: Option<(u32, u64)> = None;
    let mut current_count = 0_u32;
    let mut current_bytes = 0_u64;
    let mut current_offset = 0_u64;
    let mut current_hasher = Hasher::new();
    let mut bucket_count = 0_u64;

    while let Some(Reverse(entry)) = heap.pop() {
        let group = (entry.record.band, entry.record.hash);
        if current_key != Some(group) {
            if let Some((band, hash)) = current_key {
                finish_descriptor(
                    &mut descriptors,
                    DescriptorDraft {
                        band,
                        member_count: current_count,
                        hash,
                        relative_offset: current_offset,
                        member_bytes: current_bytes,
                        checksum: current_hasher.finalize(),
                    },
                )?;
                bucket_count = bucket_count
                    .checked_add(1)
                    .ok_or(BuildError::LengthOverflow)?;
                current_offset = current_offset
                    .checked_add(current_bytes)
                    .ok_or(BuildError::LengthOverflow)?;
            }
            current_key = Some(group);
            current_count = 0;
            current_bytes = 0;
            current_hasher = Hasher::new();
        }

        let key_bytes = entry.record.key.to_le_bytes();
        memberships.write_all(&key_bytes)?;
        current_hasher.update(&key_bytes);
        current_count = current_count
            .checked_add(1)
            .ok_or(BuildError::LengthOverflow)?;
        current_bytes = current_bytes
            .checked_add(U64_BYTES_U64)
            .ok_or(BuildError::LengthOverflow)?;

        if let Some(next) = read_spill_record(&mut readers[entry.run])? {
            heap.push(Reverse(HeapEntry {
                record: next,
                run: entry.run,
            }));
        }
    }

    if let Some((band, hash)) = current_key {
        finish_descriptor(
            &mut descriptors,
            DescriptorDraft {
                band,
                member_count: current_count,
                hash,
                relative_offset: current_offset,
                member_bytes: current_bytes,
                checksum: current_hasher.finalize(),
            },
        )?;
        bucket_count = bucket_count
            .checked_add(1)
            .ok_or(BuildError::LengthOverflow)?;
    }
    descriptors.flush()?;
    memberships.flush()?;
    descriptors.get_ref().sync_all()?;
    memberships.get_ref().sync_all()?;
    Ok(bucket_count)
}

fn assemble_bucket_payload(
    descriptor_path: &Path,
    membership_path: &Path,
    bucket_count: u64,
    output_path: &Path,
) -> Result<(u64, u32), BuildError> {
    let descriptor_bytes = bucket_count
        .checked_mul(BUCKET_RECORD_BYTES_U64)
        .ok_or(BuildError::LengthOverflow)?;
    let membership_start = BUCKET_HEADER_BYTES_U64
        .checked_add(descriptor_bytes)
        .ok_or(BuildError::LengthOverflow)?;
    let mut descriptors = BufReader::new(File::open(descriptor_path)?);
    let mut memberships = BufReader::new(File::open(membership_path)?);
    let output_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output_path)?;
    let mut output = BufWriter::new(output_file);
    let mut hasher = Hasher::new();

    let mut bucket_header = [0_u8; BUCKET_HEADER_BYTES];
    bucket_header[..8].copy_from_slice(&BUCKET_MAGIC);
    bucket_header[8..].copy_from_slice(&bucket_count.to_le_bytes());
    write_hashed(&mut output, &mut hasher, &bucket_header)?;
    for _ in 0..bucket_count {
        let draft = read_descriptor_draft(&mut descriptors)?;
        let member_offset = membership_start
            .checked_add(draft.relative_offset)
            .ok_or(BuildError::LengthOverflow)?;
        let record = encode_bucket_descriptor(draft, member_offset);
        write_hashed(&mut output, &mut hasher, &record)?;
    }
    copy_hashed(&mut memberships, &mut output, &mut hasher)?;
    output.flush()?;
    output.get_ref().sync_all()?;
    let bytes = output.get_ref().metadata()?.len();
    Ok((bytes, hasher.finalize()))
}

fn write_final_container(
    source: &mut File,
    layout: &FileLayout,
    keys: SectionDescriptor,
    bucket_payload: &Path,
    bucket_bytes: u64,
    bucket_checksum: u32,
    destination_temp: &Path,
) -> Result<(), BuildError> {
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination_temp)?;
    let mut output = BufWriter::new(file);
    output.write_all(&encode_header(layout.metadata(), 2))?;
    output.write_all(&encode_section_header(
        SectionKind::Keys,
        keys.payload_length(),
        keys.checksum(),
    ))?;
    copy_file_range(
        source,
        keys.payload_offset(),
        keys.payload_length(),
        &mut output,
    )?;
    output.write_all(&encode_section_header(
        SectionKind::Buckets,
        bucket_bytes,
        bucket_checksum,
    ))?;
    let mut buckets = BufReader::new(File::open(bucket_payload)?);
    io::copy(&mut buckets, &mut output)?;
    output.flush()?;
    output.get_ref().sync_all()?;
    Ok(())
}

fn encode_header(metadata: &IndexMetadata, section_count: u32) -> [u8; HEADER_BYTES] {
    let mut header = [0_u8; HEADER_BYTES];
    header[0..8].copy_from_slice(&FILE_MAGIC);
    header[8..10].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
    header[10..12].copy_from_slice(&HEADER_BYTES_U16.to_le_bytes());
    header[12..14].copy_from_slice(&algorithm_raw(metadata.algorithm()).to_le_bytes());
    header[14..16].copy_from_slice(&scheme_raw(metadata.signature_scheme()).to_le_bytes());
    header[16..18].copy_from_slice(&metadata.signature_scheme().width_bits().to_le_bytes());
    header[18..20].copy_from_slice(&codec_raw(metadata.key_codec()).to_le_bytes());
    header[20..24].copy_from_slice(&metadata.num_perm().to_le_bytes());
    header[24..28].copy_from_slice(&metadata.bands().to_le_bytes());
    header[28..32].copy_from_slice(&metadata.rows().to_le_bytes());
    header[32..36].copy_from_slice(&section_count.to_le_bytes());
    header[40..48].copy_from_slice(&metadata.seed().to_le_bytes());
    header[48..56].copy_from_slice(&metadata.threshold().to_le_bytes());
    header[56..64].copy_from_slice(&metadata.feature_flags().to_le_bytes());
    let checksum = crc32(&header[..64]);
    header[64..68].copy_from_slice(&checksum.to_le_bytes());
    header
}

fn encode_section_header(
    kind: SectionKind,
    payload_length: u64,
    checksum: u32,
) -> [u8; SECTION_HEADER_BYTES] {
    let mut header = [0_u8; SECTION_HEADER_BYTES];
    header[0..2].copy_from_slice(&section_kind_raw(kind).to_le_bytes());
    header[2..4].copy_from_slice(&SECTION_REQUIRED.to_le_bytes());
    header[4..12].copy_from_slice(&payload_length.to_le_bytes());
    header[12..16].copy_from_slice(&checksum.to_le_bytes());
    header
}

fn algorithm_raw(algorithm: Algorithm) -> u16 {
    match algorithm {
        Algorithm::MinHashLsh => 1,
    }
}

fn scheme_raw(scheme: SignatureScheme) -> u16 {
    match scheme {
        SignatureScheme::PariAffine32V1 => 1,
        SignatureScheme::PariAffine64V1 => 2,
    }
}

fn codec_raw(codec: CodecId) -> u16 {
    match codec {
        CodecId::Bytes => 1,
        CodecId::Utf8 => 2,
        CodecId::U64 => 3,
        CodecId::I64 => 4,
        CodecId::Json => 5,
    }
}

fn section_kind_raw(kind: SectionKind) -> u16 {
    match kind {
        SectionKind::Keys => 1,
        SectionKind::BandHashes => 2,
        SectionKind::Buckets => 3,
        SectionKind::Tombstones => 4,
    }
}

fn write_spill_record(writer: &mut impl Write, record: SpillRecord) -> Result<(), BuildError> {
    writer.write_all(&record.band.to_le_bytes())?;
    writer.write_all(&record.hash.to_le_bytes())?;
    writer.write_all(&record.key.to_le_bytes())?;
    Ok(())
}

fn read_spill_record(reader: &mut impl Read) -> Result<Option<SpillRecord>, BuildError> {
    let mut raw = [0_u8; SPILL_RECORD_BYTES];
    let mut read = 0;
    while read < raw.len() {
        match reader.read(&mut raw[read..])? {
            0 if read == 0 => return Ok(None),
            0 => {
                return Err(BuildError::InvalidSource {
                    reason: "truncated spill record",
                });
            }
            count => read += count,
        }
    }
    Ok(Some(SpillRecord {
        band: u32::from_le_bytes(
            raw[0..4]
                .try_into()
                .map_err(|_| BuildError::LengthOverflow)?,
        ),
        hash: u64::from_le_bytes(
            raw[4..12]
                .try_into()
                .map_err(|_| BuildError::LengthOverflow)?,
        ),
        key: u64::from_le_bytes(
            raw[12..20]
                .try_into()
                .map_err(|_| BuildError::LengthOverflow)?,
        ),
    }))
}

fn finish_descriptor(writer: &mut impl Write, draft: DescriptorDraft) -> Result<(), BuildError> {
    writer.write_all(&draft.band.to_le_bytes())?;
    writer.write_all(&draft.member_count.to_le_bytes())?;
    writer.write_all(&draft.hash.to_le_bytes())?;
    writer.write_all(&draft.relative_offset.to_le_bytes())?;
    writer.write_all(&draft.member_bytes.to_le_bytes())?;
    writer.write_all(&draft.checksum.to_le_bytes())?;
    writer.write_all(&0_u32.to_le_bytes())?;
    Ok(())
}

fn read_descriptor_draft(reader: &mut impl Read) -> Result<DescriptorDraft, BuildError> {
    let mut raw = [0_u8; BUCKET_RECORD_BYTES];
    reader.read_exact(&mut raw)?;
    if u32::from_le_bytes(
        raw[36..40]
            .try_into()
            .map_err(|_| BuildError::LengthOverflow)?,
    ) != 0
    {
        return Err(BuildError::InvalidSource {
            reason: "temporary descriptor reserved bytes are nonzero",
        });
    }
    Ok(DescriptorDraft {
        band: u32::from_le_bytes(
            raw[0..4]
                .try_into()
                .map_err(|_| BuildError::LengthOverflow)?,
        ),
        member_count: u32::from_le_bytes(
            raw[4..8]
                .try_into()
                .map_err(|_| BuildError::LengthOverflow)?,
        ),
        hash: u64::from_le_bytes(
            raw[8..16]
                .try_into()
                .map_err(|_| BuildError::LengthOverflow)?,
        ),
        relative_offset: u64::from_le_bytes(
            raw[16..24]
                .try_into()
                .map_err(|_| BuildError::LengthOverflow)?,
        ),
        member_bytes: u64::from_le_bytes(
            raw[24..32]
                .try_into()
                .map_err(|_| BuildError::LengthOverflow)?,
        ),
        checksum: u32::from_le_bytes(
            raw[32..36]
                .try_into()
                .map_err(|_| BuildError::LengthOverflow)?,
        ),
    })
}

fn encode_bucket_descriptor(
    draft: DescriptorDraft,
    member_offset: u64,
) -> [u8; BUCKET_RECORD_BYTES] {
    let mut output = [0_u8; BUCKET_RECORD_BYTES];
    output[0..4].copy_from_slice(&draft.band.to_le_bytes());
    output[4..8].copy_from_slice(&draft.member_count.to_le_bytes());
    output[8..16].copy_from_slice(&draft.hash.to_le_bytes());
    output[16..24].copy_from_slice(&member_offset.to_le_bytes());
    output[24..32].copy_from_slice(&draft.member_bytes.to_le_bytes());
    output[32..36].copy_from_slice(&draft.checksum.to_le_bytes());
    output
}

fn write_hashed(
    writer: &mut impl Write,
    hasher: &mut Hasher,
    bytes: &[u8],
) -> Result<(), BuildError> {
    writer.write_all(bytes)?;
    hasher.update(bytes);
    Ok(())
}

fn copy_hashed(
    reader: &mut impl Read,
    writer: &mut impl Write,
    hasher: &mut Hasher,
) -> Result<(), BuildError> {
    let mut buffer = vec![0_u8; COPY_BUFFER_BYTES];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        writer.write_all(&buffer[..count])?;
        hasher.update(&buffer[..count]);
    }
    Ok(())
}

fn copy_file_range(
    source: &mut File,
    offset: u64,
    length: u64,
    writer: &mut impl Write,
) -> Result<(), BuildError> {
    source.seek(SeekFrom::Start(offset))?;
    let mut remaining = length;
    let mut buffer = vec![0_u8; COPY_BUFFER_BYTES];
    while remaining > 0 {
        let count = usize::try_from(remaining.min(COPY_BUFFER_BYTES_U64))
            .map_err(|_| BuildError::LengthOverflow)?;
        source.read_exact(&mut buffer[..count])?;
        writer.write_all(&buffer[..count])?;
        remaining -= u64::try_from(count).map_err(|_| BuildError::LengthOverflow)?;
    }
    Ok(())
}

fn build_nonce() -> Result<u128, BuildError> {
    let duration =
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| BuildError::InvalidSource {
                reason: "system clock is before UNIX epoch",
            })?;
    Ok(duration.as_nanos() ^ u128::from(std::process::id()))
}

fn temp_path(destination: &Path, nonce: u128, suffix: &str) -> PathBuf {
    let name = destination
        .file_name()
        .map_or_else(|| "pari".into(), |name| name.to_string_lossy());
    destination.with_file_name(format!(".{name}.{nonce:032x}.{suffix}.tmp"))
}

fn sync_parent(path: &Path) -> Result<(), BuildError> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
    };

    use pari_core::MinHash32;
    use pari_store::PersistentIndex32;
    use pari_store_lazy::{build_from_snapshot, LazyIndex32};

    use super::{build_external, BuildError, BuildOptions};

    fn test_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "pari-external-{name}-{}-{}.idx",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ))
    }

    fn cleanup(path: &Path) {
        let _ = fs::remove_file(path);
    }

    fn sketch(base: u64) -> MinHash32 {
        let mut sketch = MinHash32::new(128, 7).expect("valid sketch");
        for value in base..base + 40 {
            sketch.update(&value.to_le_bytes());
        }
        sketch
    }

    fn source_fixture(name: &str) -> (PathBuf, Vec<MinHash32>) {
        let source = test_path(&format!("{name}-source"));
        cleanup(&source);
        let sketches = (0_u64..32)
            .map(|item| sketch(item.saturating_mul(1_000)))
            .collect::<Vec<_>>();
        let mut store = PersistentIndex32::create(&source, 0.8, 128, 7).expect("create source");
        store
            .insert_many(
                sketches
                    .iter()
                    .enumerate()
                    .map(|(key, sketch)| (u64::try_from(key).expect("test key fits u64"), sketch)),
            )
            .expect("insert source");
        store.sync().expect("sync source");
        (source, sketches)
    }

    #[test]
    fn tiny_spills_are_byte_identical_to_reference_builder() {
        let (source, _) = source_fixture("identical");
        let reference = test_path("identical-reference");
        let external = test_path("identical-external");
        cleanup(&reference);
        cleanup(&external);
        build_from_snapshot(&source, &reference).expect("reference build");
        let stats = build_external(
            &source,
            &external,
            BuildOptions {
                max_buffer_records: 7,
            },
        )
        .expect("external build");
        assert!(stats.spill_runs > 1);
        assert!(stats.peak_buffered_records <= 7);
        assert_eq!(
            fs::read(&external).expect("external bytes"),
            fs::read(&reference).expect("reference bytes")
        );
        cleanup(&source);
        cleanup(&reference);
        cleanup(&external);
    }

    #[test]
    fn externally_built_queries_match_phase1() {
        let (source, sketches) = source_fixture("query");
        let external = test_path("query-external");
        cleanup(&external);
        build_external(
            &source,
            &external,
            BuildOptions {
                max_buffer_records: 11,
            },
        )
        .expect("external build");
        let phase1 = PersistentIndex32::open(&source).expect("phase1");
        let mut lazy = LazyIndex32::open(&external).expect("lazy");
        for query in sketches.iter().take(8) {
            assert_eq!(
                lazy.query(query).expect("lazy query"),
                phase1.query(query).expect("phase1 query")
            );
        }
        cleanup(&source);
        cleanup(&external);
    }

    #[test]
    fn zero_buffer_is_rejected_without_destination() {
        let (source, _) = source_fixture("zero");
        let destination = test_path("zero-output");
        cleanup(&destination);
        assert!(matches!(
            build_external(
                &source,
                &destination,
                BuildOptions {
                    max_buffer_records: 0,
                },
            ),
            Err(BuildError::InvalidOptions { .. })
        ));
        assert!(!destination.exists());
        cleanup(&source);
    }
}
