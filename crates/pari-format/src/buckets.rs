use std::{
    collections::BTreeMap,
    error::Error,
    fmt,
    io::{Read, Seek},
    sync::{mpsc, Mutex},
};

use crc32fast::hash as crc32;

use crate::{FileLayout, LayoutError, SectionDescriptor, SectionKind};

const BUCKET_SEGMENT_MAGIC: [u8; 8] = *b"PARIBKT\0";
const BUCKET_SEGMENT_VERSION: u16 = 1;
/// Fixed byte width of a version-1 bucket-segment header.
pub const BUCKET_SEGMENT_HEADER_BYTES: usize = 40;
const BUCKET_SEGMENT_HEADER_BYTES_U16: u16 = 40;
const BUCKET_SEGMENT_HEADER_BYTES_U64: u64 = 40;
const BUCKET_DIRECTORY_ENTRY_BYTES: usize = 32;
const U64_BYTES: usize = 8;
const U64_BYTES_U64: u64 = 8;

/// Target payload size used when splitting large bucket collections into
/// independently checksummed sections.
pub const BUCKET_SEGMENT_TARGET_BYTES: usize = 32 * 1024 * 1024;

/// Stable `(band, hash)` identity of one LSH bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct BucketKey {
    band: u32,
    hash: u64,
}

impl BucketKey {
    /// Construct a bucket identity.
    #[must_use]
    pub const fn new(band: u32, hash: u64) -> Self {
        Self { band, hash }
    }

    /// Return the zero-based LSH band number.
    #[must_use]
    pub const fn band(self) -> u32 {
        self.band
    }

    /// Return the stable hash of the values in this band.
    #[must_use]
    pub const fn hash(self) -> u64 {
        self.hash
    }
}

/// Borrowed bucket record used by the encoder.
#[derive(Debug, Clone, Copy)]
pub struct BucketRecord<'a> {
    key: BucketKey,
    members: &'a [u64],
}

impl<'a> BucketRecord<'a> {
    /// Construct one sorted bucket record.
    #[must_use]
    pub const fn new(key: BucketKey, members: &'a [u64]) -> Self {
        Self { key, members }
    }

    /// Return the bucket identity.
    #[must_use]
    pub const fn key(self) -> BucketKey {
        self.key
    }

    /// Borrow the member keys.
    #[must_use]
    pub const fn members(self) -> &'a [u64] {
        self.members
    }
}

/// Exact location of one bucket's member list inside a `Buckets` section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BucketLocation {
    key: BucketKey,
    section: SectionDescriptor,
    member_offset: u64,
    member_count: u32,
    member_checksum: u32,
}

impl BucketLocation {
    /// Return the bucket identity.
    #[must_use]
    pub const fn key(self) -> BucketKey {
        self.key
    }

    /// Return the containing `Buckets` section.
    #[must_use]
    pub const fn section(self) -> SectionDescriptor {
        self.section
    }

    /// Return the member-list offset relative to the section payload.
    #[must_use]
    pub const fn member_offset(self) -> u64 {
        self.member_offset
    }

    /// Return the number of member keys.
    #[must_use]
    pub const fn member_count(self) -> u32 {
        self.member_count
    }

    /// Return the CRC32 of the encoded member list.
    #[must_use]
    pub const fn member_checksum(self) -> u32 {
        self.member_checksum
    }
}

/// Errors produced by the stable bucket-segment codec.
#[derive(Debug)]
pub enum BucketError {
    /// File I/O or outer-container range validation failed.
    Layout(LayoutError),
    /// Bucket bytes violate a structural invariant.
    Invalid { reason: &'static str },
    /// A checksummed bucket directory is corrupt.
    DirectoryChecksumMismatch { expected: u32, actual: u32 },
    /// A checksummed bucket member list is corrupt.
    MemberChecksumMismatch { expected: u32, actual: u32 },
    /// Checked length arithmetic or a platform conversion overflowed.
    LengthOverflow,
}

impl fmt::Display for BucketError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Layout(error) => error.fmt(formatter),
            Self::Invalid { reason } => write!(formatter, "invalid bucket segment: {reason}"),
            Self::DirectoryChecksumMismatch { expected, actual } => write!(
                formatter,
                "bucket directory checksum mismatch: stored {expected:#010x}, computed {actual:#010x}"
            ),
            Self::MemberChecksumMismatch { expected, actual } => write!(
                formatter,
                "bucket member checksum mismatch: stored {expected:#010x}, computed {actual:#010x}"
            ),
            Self::LengthOverflow => formatter.write_str("bucket segment length arithmetic overflowed"),
        }
    }
}

impl Error for BucketError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Layout(error) => Some(error),
            _ => None,
        }
    }
}

impl From<LayoutError> for BucketError {
    fn from(error: LayoutError) -> Self {
        Self::Layout(error)
    }
}

/// Return the encoded contribution of one bucket to a segment payload.
pub fn bucket_record_size(member_count: usize) -> Result<usize, BucketError> {
    BUCKET_DIRECTORY_ENTRY_BYTES
        .checked_add(
            member_count
                .checked_mul(U64_BYTES)
                .ok_or(BucketError::LengthOverflow)?,
        )
        .ok_or(BucketError::LengthOverflow)
}

/// Encode one sorted bucket segment using Pari's stable checksummed layout.
///
/// Records must be strictly ordered by `(band, hash)`. Empty segments are
/// valid and are used for empty indexes.
pub fn encode_bucket_segment(records: &[BucketRecord<'_>]) -> Result<Vec<u8>, BucketError> {
    validate_record_order(records)?;
    let directory_bytes = records
        .len()
        .checked_mul(BUCKET_DIRECTORY_ENTRY_BYTES)
        .ok_or(BucketError::LengthOverflow)?;
    let member_bytes = records.iter().try_fold(0_usize, |total, record| {
        total
            .checked_add(
                record
                    .members
                    .len()
                    .checked_mul(U64_BYTES)
                    .ok_or(BucketError::LengthOverflow)?,
            )
            .ok_or(BucketError::LengthOverflow)
    })?;
    let total = BUCKET_SEGMENT_HEADER_BYTES
        .checked_add(directory_bytes)
        .and_then(|value| value.checked_add(member_bytes))
        .ok_or(BucketError::LengthOverflow)?;

    let member_start = BUCKET_SEGMENT_HEADER_BYTES
        .checked_add(directory_bytes)
        .ok_or(BucketError::LengthOverflow)?;
    let mut directory = Vec::with_capacity(directory_bytes);
    let mut members_payload = Vec::with_capacity(member_bytes);
    let mut member_offset = u64::try_from(member_start).map_err(|_| BucketError::LengthOverflow)?;

    for record in records {
        let start = members_payload.len();
        for member in record.members {
            members_payload.extend_from_slice(&member.to_le_bytes());
        }
        let member_count =
            u32::try_from(record.members.len()).map_err(|_| BucketError::LengthOverflow)?;
        directory.extend_from_slice(&record.key.band.to_le_bytes());
        directory.extend_from_slice(&0_u32.to_le_bytes());
        directory.extend_from_slice(&record.key.hash.to_le_bytes());
        directory.extend_from_slice(&member_offset.to_le_bytes());
        directory.extend_from_slice(&member_count.to_le_bytes());
        directory.extend_from_slice(&crc32(&members_payload[start..]).to_le_bytes());
        member_offset = member_offset
            .checked_add(
                u64::from(member_count)
                    .checked_mul(U64_BYTES_U64)
                    .ok_or(BucketError::LengthOverflow)?,
            )
            .ok_or(BucketError::LengthOverflow)?;
    }

    let mut header = [0_u8; BUCKET_SEGMENT_HEADER_BYTES];
    header[..8].copy_from_slice(&BUCKET_SEGMENT_MAGIC);
    header[8..10].copy_from_slice(&BUCKET_SEGMENT_VERSION.to_le_bytes());
    header[10..12].copy_from_slice(&BUCKET_SEGMENT_HEADER_BYTES_U16.to_le_bytes());
    header[12..20].copy_from_slice(
        &u64::try_from(records.len())
            .map_err(|_| BucketError::LengthOverflow)?
            .to_le_bytes(),
    );
    header[20..28].copy_from_slice(
        &u64::try_from(directory_bytes)
            .map_err(|_| BucketError::LengthOverflow)?
            .to_le_bytes(),
    );
    header[28..32].copy_from_slice(&crc32(&directory).to_le_bytes());
    let header_checksum = crc32(&header[..32]);
    header[32..36].copy_from_slice(&header_checksum.to_le_bytes());

    let mut output = Vec::with_capacity(total);
    output.extend_from_slice(&header);
    output.extend_from_slice(&directory);
    output.extend_from_slice(&members_payload);
    Ok(output)
}

/// Decode and validate one `Buckets` section without reading member payloads.
pub fn decode_bucket_segment<R: Read + Seek>(
    layout: &FileLayout,
    reader: &mut R,
    descriptor: SectionDescriptor,
    bands: usize,
) -> Result<Vec<BucketLocation>, BucketError> {
    let directory = read_segment_directory(layout, reader, descriptor)?;
    let mut locations = Vec::with_capacity(directory.count);
    visit_directory_entries(descriptor, &directory, bands, |location| {
        locations.push(location);
        Ok(())
    })?;
    Ok(locations)
}

/// A checksum-verified bucket directory read from one `Buckets` section.
struct SegmentDirectory {
    bytes: Vec<u8>,
    count: usize,
    member_start: u64,
}

/// Validate a segment header and read its checksum-verified directory.
fn read_segment_directory<R: Read + Seek>(
    layout: &FileLayout,
    reader: &mut R,
    descriptor: SectionDescriptor,
) -> Result<SegmentDirectory, BucketError> {
    parse_segment_directory(descriptor, |offset, length| {
        Ok(layout.read_section_range(reader, descriptor, offset, length)?)
    })
}

/// Validate a segment header and its checksum-verified directory, reading
/// section byte ranges through `read`.
fn parse_segment_directory(
    descriptor: SectionDescriptor,
    mut read: impl FnMut(u64, usize) -> Result<Vec<u8>, BucketError>,
) -> Result<SegmentDirectory, BucketError> {
    if descriptor.kind() != SectionKind::Buckets || !descriptor.required() {
        return Err(BucketError::Invalid {
            reason: "bucket segment must be a required Buckets section",
        });
    }
    if descriptor.payload_length() < BUCKET_SEGMENT_HEADER_BYTES_U64 {
        return Err(BucketError::Invalid {
            reason: "bucket segment header is truncated",
        });
    }

    let header = read(0, BUCKET_SEGMENT_HEADER_BYTES)?;
    validate_header_fixed_fields(&header)?;
    let expected_header_checksum = read_u32_at(&header, 32)?;
    let actual_header_checksum = crc32(&header[..32]);
    if expected_header_checksum != actual_header_checksum {
        return Err(BucketError::DirectoryChecksumMismatch {
            expected: expected_header_checksum,
            actual: actual_header_checksum,
        });
    }

    let count =
        usize::try_from(read_u64_at(&header, 12)?).map_err(|_| BucketError::LengthOverflow)?;
    let directory_bytes =
        usize::try_from(read_u64_at(&header, 20)?).map_err(|_| BucketError::LengthOverflow)?;
    let expected_directory_bytes = count
        .checked_mul(BUCKET_DIRECTORY_ENTRY_BYTES)
        .ok_or(BucketError::LengthOverflow)?;
    if directory_bytes != expected_directory_bytes {
        return Err(BucketError::Invalid {
            reason: "bucket directory byte length does not match its record count",
        });
    }
    let member_start = BUCKET_SEGMENT_HEADER_BYTES
        .checked_add(directory_bytes)
        .ok_or(BucketError::LengthOverflow)?;
    let member_start_u64 = u64::try_from(member_start).map_err(|_| BucketError::LengthOverflow)?;
    if member_start_u64 > descriptor.payload_length() {
        return Err(BucketError::Invalid {
            reason: "bucket directory extends past the section payload",
        });
    }

    let directory = read(BUCKET_SEGMENT_HEADER_BYTES_U64, directory_bytes)?;
    let expected_directory_checksum = read_u32_at(&header, 28)?;
    let actual_directory_checksum = crc32(&directory);
    if expected_directory_checksum != actual_directory_checksum {
        return Err(BucketError::DirectoryChecksumMismatch {
            expected: expected_directory_checksum,
            actual: actual_directory_checksum,
        });
    }

    Ok(SegmentDirectory {
        bytes: directory,
        count,
        member_start: member_start_u64,
    })
}

/// Verify a bucket member range and return its decoded keys.
pub fn read_bucket_members<R: Read + Seek>(
    layout: &FileLayout,
    reader: &mut R,
    location: BucketLocation,
) -> Result<Vec<u64>, BucketError> {
    let byte_length = usize::try_from(location.member_count)
        .map_err(|_| BucketError::LengthOverflow)?
        .checked_mul(U64_BYTES)
        .ok_or(BucketError::LengthOverflow)?;
    let bytes = layout.read_section_range(
        reader,
        location.section,
        location.member_offset,
        byte_length,
    )?;
    let actual = crc32(&bytes);
    if actual != location.member_checksum {
        return Err(BucketError::MemberChecksumMismatch {
            expected: location.member_checksum,
            actual,
        });
    }
    bytes.chunks_exact(U64_BYTES).map(read_u64).collect()
}

/// Ensure locations from multiple bucket sections remain globally sorted and
/// contain no duplicate `(band, hash)` keys.
pub fn validate_global_bucket_order(locations: &[BucketLocation]) -> Result<(), BucketError> {
    for pair in locations.windows(2) {
        if pair[0].key >= pair[1].key {
            return Err(BucketError::Invalid {
                reason: "bucket directory contains duplicate or unsorted bucket keys",
            });
        }
    }
    Ok(())
}

fn validate_record_order(records: &[BucketRecord<'_>]) -> Result<(), BucketError> {
    for pair in records.windows(2) {
        if pair[0].key >= pair[1].key {
            return Err(BucketError::Invalid {
                reason: "bucket records must be strictly sorted and unique",
            });
        }
    }
    Ok(())
}

fn validate_header_fixed_fields(header: &[u8]) -> Result<(), BucketError> {
    if header.get(..8) != Some(BUCKET_SEGMENT_MAGIC.as_slice()) {
        return Err(BucketError::Invalid {
            reason: "invalid bucket segment magic",
        });
    }
    if read_u16_at(header, 8)? != BUCKET_SEGMENT_VERSION {
        return Err(BucketError::Invalid {
            reason: "unsupported bucket segment version",
        });
    }
    if read_u16_at(header, 10)? != BUCKET_SEGMENT_HEADER_BYTES_U16 {
        return Err(BucketError::Invalid {
            reason: "invalid bucket segment header size",
        });
    }
    if header.get(36..40) != Some(&[0, 0, 0, 0]) {
        return Err(BucketError::Invalid {
            reason: "bucket segment reserved bytes must be zero",
        });
    }
    Ok(())
}

/// Validate every entry of a verified directory in order, passing each to `visit`.
fn visit_directory_entries(
    descriptor: SectionDescriptor,
    directory: &SegmentDirectory,
    bands: usize,
    mut visit: impl FnMut(BucketLocation) -> Result<(), BucketError>,
) -> Result<(), BucketError> {
    let bands_u32 = u32::try_from(bands).map_err(|_| BucketError::LengthOverflow)?;
    let mut visited = 0_usize;
    let mut expected_member_offset = directory.member_start;
    let mut previous_key = None;
    for entry in directory.bytes.chunks_exact(BUCKET_DIRECTORY_ENTRY_BYTES) {
        if read_u32_at(entry, 4)? != 0 {
            return Err(BucketError::Invalid {
                reason: "bucket directory reserved bytes must be zero",
            });
        }
        let key = BucketKey {
            band: read_u32_at(entry, 0)?,
            hash: read_u64_at(entry, 8)?,
        };
        if key.band >= bands_u32 {
            return Err(BucketError::Invalid {
                reason: "bucket directory band exceeds index metadata",
            });
        }
        if previous_key.is_some_and(|previous| previous >= key) {
            return Err(BucketError::Invalid {
                reason: "bucket segment keys are duplicated or unsorted",
            });
        }
        let member_offset = read_u64_at(entry, 16)?;
        if member_offset != expected_member_offset {
            return Err(BucketError::Invalid {
                reason: "bucket member ranges contain a gap, overlap, or invalid offset",
            });
        }
        let member_count = read_u32_at(entry, 24)?;
        let member_end = member_offset
            .checked_add(
                u64::from(member_count)
                    .checked_mul(U64_BYTES_U64)
                    .ok_or(BucketError::LengthOverflow)?,
            )
            .ok_or(BucketError::LengthOverflow)?;
        if member_end > descriptor.payload_length() {
            return Err(BucketError::Invalid {
                reason: "bucket member range extends past the section payload",
            });
        }
        let location = BucketLocation {
            key,
            section: descriptor,
            member_offset,
            member_count,
            member_checksum: read_u32_at(entry, 28)?,
        };
        expected_member_offset = member_end;
        previous_key = Some(key);
        visited += 1;
        visit(location)?;
    }
    if visited != directory.count {
        return Err(BucketError::Invalid {
            reason: "bucket directory record count is inconsistent",
        });
    }
    if expected_member_offset != descriptor.payload_length() {
        return Err(BucketError::Invalid {
            reason: "bucket segment has trailing member bytes",
        });
    }
    Ok(())
}

/// Maximum directory entries summarized by one in-memory fence (2 KiB on disk).
const DIRECTORY_BLOCK_ENTRIES: usize = 64;

/// Compact, checksummed index over every bucket directory in a snapshot.
///
/// [`BucketDirectory::read`] validates every directory exactly as
/// [`decode_bucket_segment`] and [`validate_global_bucket_order`] do, but keeps
/// only the high 32 bits of each bucket's hash, the first key and CRC32 of each
/// block of up to 64 same-band directory entries, and an exact histogram of
/// bucket sizes: about 4.6 bytes per bucket instead of a full
/// [`BucketLocation`]. [`BucketDirectory::find`] answers most absent keys from
/// memory and otherwise reads the one directory block that can hold the key,
/// verifying its checksum before using it.
#[derive(Debug, Clone)]
#[cfg_attr(test, derive(PartialEq, Eq))]
pub struct BucketDirectory {
    sections: Vec<SectionDescriptor>,
    blocks: Vec<DirectoryBlock>,
    /// High 32 bits of every bucket hash, in directory order.
    prefixes: Vec<u32>,
    buckets: u64,
    size_counts: Vec<(u32, u64)>,
}

#[derive(Debug, Clone, Copy)]
#[cfg_attr(test, derive(PartialEq, Eq))]
struct DirectoryBlock {
    first_key: BucketKey,
    section: u32,
    first_entry: u32,
    entries: u32,
    checksum: u32,
    /// Index of the block's first bucket in `prefixes`.
    first_prefix: u64,
}

/// Bucket sections below which [`BucketDirectory::read`] validates on the
/// calling thread.
/// Tests use a small limit so every multi-section directory they read is
/// validated on several threads.
const PARALLEL_DIRECTORY_MIN_BYTES: u64 = if cfg!(test) { 1 } else { 64 * 1024 * 1024 };

/// Most threads [`BucketDirectory::read`] validates sections on. Each holds
/// one section's directory, so this also bounds the directories in memory.
const MAX_DIRECTORY_WORKERS: usize = 4;

/// Threads to validate `descriptors` on: none for small indexes.
fn directory_workers(descriptors: &[SectionDescriptor]) -> usize {
    let bytes: u64 = descriptors
        .iter()
        .map(|descriptor| descriptor.payload_length())
        .fold(0, u64::saturating_add);
    if descriptors.len() < 2 || bytes < PARALLEL_DIRECTORY_MIN_BYTES {
        return 0;
    }
    std::thread::available_parallelism()
        .map_or(1, std::num::NonZeroUsize::get)
        .min(MAX_DIRECTORY_WORKERS)
        .min(descriptors.len())
}

/// Read section directories in file order on this thread while worker
/// threads validate and index them, appending results in section order.
///
/// At most one read directory waits for a worker, so at most
/// `workers + 1` directories are held at once.
fn read_sections_in_parallel<R: Read + Seek>(
    layout: &FileLayout,
    reader: &mut R,
    descriptors: &[SectionDescriptor],
    bands: usize,
    workers: usize,
    assembler: &mut DirectoryAssembler,
) -> Result<(), BucketError> {
    let (tasks, task_queue) = mpsc::sync_channel::<(usize, SegmentDirectory)>(1);
    let task_queue = Mutex::new(task_queue);
    let (results, finished) = mpsc::channel::<(usize, SectionResult)>();
    std::thread::scope(|scope| {
        for _ in 0..workers {
            let results = results.clone();
            let task_queue = &task_queue;
            scope.spawn(move || loop {
                let task = match task_queue.lock() {
                    Ok(queue) => queue.recv(),
                    Err(_) => return,
                };
                let Ok((section_index, directory)) = task else {
                    return;
                };
                let indexed =
                    index_section(section_index, descriptors[section_index], &directory, bands);
                drop(directory);
                if results.send((section_index, indexed)).is_err() {
                    return;
                }
            });
        }
        drop(results);

        // Results arrive out of order; append them in section order.
        let mut pending = BTreeMap::new();
        let mut next = 0;
        let mut outcome = Ok(());
        let mut drain = |pending: &mut BTreeMap<usize, SectionResult>,
                         next: &mut usize,
                         outcome: &mut Result<(), BucketError>| {
            while let Some(indexed) = pending.remove(next) {
                if outcome.is_ok() {
                    *outcome = assembler.push(indexed);
                }
                *next += 1;
            }
        };
        let mut read_error = None;
        for (section_index, descriptor) in descriptors.iter().copied().enumerate() {
            match read_segment_directory(layout, reader, descriptor) {
                Ok(directory) => {
                    if tasks.send((section_index, directory)).is_err() {
                        break;
                    }
                }
                Err(error) => {
                    read_error = Some((section_index, error));
                    break;
                }
            }
            for (index, indexed) in finished.try_iter() {
                pending.insert(index, indexed);
            }
            drain(&mut pending, &mut next, &mut outcome);
            if outcome.is_err() {
                break;
            }
        }
        drop(tasks);
        for (index, indexed) in &finished {
            pending.insert(index, indexed);
        }
        drain(&mut pending, &mut next, &mut outcome);
        outcome?;
        // A read error belongs after every section before it.
        match read_error {
            Some((section_index, error)) if section_index == next => Err(error),
            _ => Ok(()),
        }
    })
}

/// One section's share of a [`BucketDirectory`], or why it is invalid.
type SectionResult = Result<SectionIndex, SectionFailure>;

/// The blocks, hash prefixes and bucket sizes of one validated section.
struct SectionIndex {
    /// Blocks whose `first_prefix` counts from this section's first bucket.
    blocks: Vec<DirectoryBlock>,
    prefixes: Vec<u32>,
    sizes: SizeCounts,
    buckets: u64,
    first: Option<BucketKey>,
    last: Option<BucketKey>,
}

/// A section that failed validation. `first` is its first bucket key when the
/// first directory entry itself was valid, so global ordering is checked
/// against it first, as a sequential read would.
struct SectionFailure {
    first: Option<BucketKey>,
    error: BucketError,
}

/// Validate one section's directory and index it.
fn index_section(
    section_index: usize,
    descriptor: SectionDescriptor,
    directory: &SegmentDirectory,
    bands: usize,
) -> SectionResult {
    let mut sizes = SizeCounts::default();
    let mut first = None;
    let mut last = None;
    let visited = visit_directory_entries(descriptor, directory, bands, |location| {
        first.get_or_insert(location.key);
        last = Some(location.key);
        sizes.add(location.member_count)
    });
    let fail = |first, error| SectionFailure { first, error };
    if let Err(error) = visited {
        return Err(fail(first, error));
    }
    let blocks_and_prefixes = || -> Result<(Vec<DirectoryBlock>, Vec<u32>), BucketError> {
        let section = u32::try_from(section_index).map_err(|_| BucketError::LengthOverflow)?;
        let entries = directory.count;
        let band_at =
            |entry: usize| read_u32_at(&directory.bytes, entry * BUCKET_DIRECTORY_ENTRY_BYTES);
        let mut blocks = Vec::with_capacity(entries.div_ceil(DIRECTORY_BLOCK_ENTRIES));
        let mut prefixes = Vec::with_capacity(entries);
        // Blocks never span a band, so the hash prefixes of one block are
        // sorted and an absent key can be rejected without reading it.
        let mut start = 0;
        while start < entries {
            let band = band_at(start)?;
            let mut end = start + 1;
            while end < entries && end - start < DIRECTORY_BLOCK_ENTRIES && band_at(end)? == band {
                end += 1;
            }
            let bytes = &directory.bytes
                [start * BUCKET_DIRECTORY_ENTRY_BYTES..end * BUCKET_DIRECTORY_ENTRY_BYTES];
            blocks.push(DirectoryBlock {
                first_key: entry_key(bytes)?,
                section,
                first_entry: u32::try_from(start).map_err(|_| BucketError::LengthOverflow)?,
                entries: u32::try_from(end - start).map_err(|_| BucketError::LengthOverflow)?,
                checksum: crc32(bytes),
                first_prefix: u64::try_from(prefixes.len())
                    .map_err(|_| BucketError::LengthOverflow)?,
            });
            for entry in bytes.chunks_exact(BUCKET_DIRECTORY_ENTRY_BYTES) {
                prefixes.push(hash_prefix(read_u64_at(entry, 8)?));
            }
            start = end;
        }
        Ok((blocks, prefixes))
    };
    let (blocks, prefixes) = blocks_and_prefixes().map_err(|error| fail(first, error))?;
    Ok(SectionIndex {
        blocks,
        prefixes,
        sizes,
        buckets: u64::try_from(directory.count)
            .map_err(|_| fail(first, BucketError::LengthOverflow))?,
        first,
        last,
    })
}

/// Joins section indexes, in section order, into one [`BucketDirectory`].
#[derive(Default)]
struct DirectoryAssembler {
    blocks: Vec<DirectoryBlock>,
    prefixes: Vec<u32>,
    sizes: SizeCounts,
    buckets: u64,
    previous: Option<BucketKey>,
}

impl DirectoryAssembler {
    /// Append the next section, or report why it, or its order after the
    /// previous sections, is invalid.
    fn push(&mut self, indexed: SectionResult) -> Result<(), BucketError> {
        let first = match &indexed {
            Ok(index) => index.first,
            Err(failure) => failure.first,
        };
        if let (Some(previous), Some(first)) = (self.previous, first) {
            if previous >= first {
                return Err(BucketError::Invalid {
                    reason: "bucket directory contains duplicate or unsorted bucket keys",
                });
            }
        }
        let index = indexed.map_err(|failure| failure.error)?;
        let offset = u64::try_from(self.prefixes.len()).map_err(|_| BucketError::LengthOverflow)?;
        self.blocks
            .extend(index.blocks.into_iter().map(|block| DirectoryBlock {
                first_prefix: block.first_prefix + offset,
                ..block
            }));
        self.prefixes.extend_from_slice(&index.prefixes);
        self.sizes.merge(&index.sizes)?;
        self.buckets = self
            .buckets
            .checked_add(index.buckets)
            .ok_or(BucketError::LengthOverflow)?;
        if index.last.is_some() {
            self.previous = index.last;
        }
        Ok(())
    }

    fn finish(mut self, descriptors: &[SectionDescriptor]) -> BucketDirectory {
        self.blocks.shrink_to_fit();
        self.prefixes.shrink_to_fit();
        BucketDirectory {
            sections: descriptors.to_vec(),
            blocks: self.blocks,
            prefixes: self.prefixes,
            buckets: self.buckets,
            size_counts: self.sizes.into_sorted(),
        }
    }
}

/// Bucket sizes counted by [`BucketDirectory::read`]: most buckets hold a few
/// members, so small sizes are counted in an array and only larger ones in a
/// map.
struct SizeCounts {
    small: [u64; SMALL_BUCKET_SIZES],
    large: BTreeMap<u32, u64>,
}

/// Bucket sizes below this are counted in [`SizeCounts::small`].
const SMALL_BUCKET_SIZES: usize = 64;

impl Default for SizeCounts {
    fn default() -> Self {
        Self {
            small: [0; SMALL_BUCKET_SIZES],
            large: BTreeMap::new(),
        }
    }
}

impl SizeCounts {
    fn add(&mut self, size: u32) -> Result<(), BucketError> {
        self.add_many(size, 1)
    }

    fn add_many(&mut self, size: u32, buckets: u64) -> Result<(), BucketError> {
        let count = match usize::try_from(size)
            .ok()
            .and_then(|index| self.small.get_mut(index))
        {
            Some(count) => count,
            None => self.large.entry(size).or_insert(0),
        };
        *count = count
            .checked_add(buckets)
            .ok_or(BucketError::LengthOverflow)?;
        Ok(())
    }

    fn merge(&mut self, other: &Self) -> Result<(), BucketError> {
        for (size, buckets) in other.sorted() {
            self.add_many(size, buckets)?;
        }
        Ok(())
    }

    /// `(size, buckets)` pairs for every size seen, in ascending size order.
    fn sorted(&self) -> impl Iterator<Item = (u32, u64)> + '_ {
        (0_u32..)
            .zip(self.small.iter().copied())
            .filter(|&(_, count)| count > 0)
            .chain(self.large.iter().map(|(&size, &count)| (size, count)))
    }

    fn into_sorted(self) -> Vec<(u32, u64)> {
        self.sorted().collect()
    }
}

fn hash_prefix(hash: u64) -> u32 {
    u32::try_from(hash >> 32).unwrap_or(u32::MAX)
}

impl BucketDirectory {
    /// Validate the directories of `descriptors`, in file order, and index them.
    ///
    /// Directory bytes are read one section at a time; member payloads are
    /// not read.
    pub fn read<R: Read + Seek>(
        layout: &FileLayout,
        reader: &mut R,
        descriptors: &[SectionDescriptor],
        bands: usize,
    ) -> Result<Self, BucketError> {
        Self::read_with_workers(
            layout,
            reader,
            descriptors,
            bands,
            directory_workers(descriptors),
        )
    }

    /// [`Self::read`] validating on `workers` threads, or on the calling
    /// thread when fewer than two.
    fn read_with_workers<R: Read + Seek>(
        layout: &FileLayout,
        reader: &mut R,
        descriptors: &[SectionDescriptor],
        bands: usize,
        workers: usize,
    ) -> Result<Self, BucketError> {
        let mut assembler = DirectoryAssembler::default();
        if workers < 2 {
            for (section_index, descriptor) in descriptors.iter().copied().enumerate() {
                let directory = read_segment_directory(layout, reader, descriptor)?;
                assembler.push(index_section(section_index, descriptor, &directory, bands))?;
            }
        } else {
            read_sections_in_parallel(layout, reader, descriptors, bands, workers, &mut assembler)?;
        }
        Ok(assembler.finish(descriptors))
    }

    /// Return the number of indexed buckets.
    #[must_use]
    pub const fn len(&self) -> u64 {
        self.buckets
    }

    /// Return whether no buckets are indexed.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.buckets == 0
    }

    /// Return `(member count, number of buckets)` pairs in ascending size order.
    pub fn size_counts(&self) -> impl Iterator<Item = (u32, u64)> + '_ {
        self.size_counts.iter().copied()
    }

    /// Locate one bucket by reading and verifying its directory block.
    pub fn find<R: Read + Seek>(
        &self,
        layout: &FileLayout,
        reader: &mut R,
        key: BucketKey,
    ) -> Result<Option<BucketLocation>, BucketError> {
        let Some(block) = self
            .blocks
            .partition_point(|block| block.first_key <= key)
            .checked_sub(1)
            .map(|index| self.blocks[index])
        else {
            return Ok(None);
        };
        if block.first_key.band != key.band {
            return Ok(None);
        }
        let first = usize::try_from(block.first_prefix).map_err(|_| BucketError::LengthOverflow)?;
        let entries = usize::try_from(block.entries).map_err(|_| BucketError::LengthOverflow)?;
        let prefixes = self
            .prefixes
            .get(
                first
                    ..first
                        .checked_add(entries)
                        .ok_or(BucketError::LengthOverflow)?,
            )
            .ok_or(BucketError::Invalid {
                reason: "bucket directory block references missing hash prefixes",
            })?;
        let prefix = hash_prefix(key.hash);
        if prefixes.binary_search(&prefix).is_err() {
            return Ok(None);
        }
        let descriptor = *self
            .sections
            .get(usize::try_from(block.section).map_err(|_| BucketError::LengthOverflow)?)
            .ok_or(BucketError::Invalid {
                reason: "bucket directory block references a missing section",
            })?;
        let entry_bytes =
            u64::try_from(BUCKET_DIRECTORY_ENTRY_BYTES).map_err(|_| BucketError::LengthOverflow)?;
        let offset = u64::from(block.first_entry)
            .checked_mul(entry_bytes)
            .and_then(|bytes| bytes.checked_add(BUCKET_SEGMENT_HEADER_BYTES_U64))
            .ok_or(BucketError::LengthOverflow)?;
        let length = usize::try_from(block.entries)
            .map_err(|_| BucketError::LengthOverflow)?
            .checked_mul(BUCKET_DIRECTORY_ENTRY_BYTES)
            .ok_or(BucketError::LengthOverflow)?;
        let bytes = layout.read_section_range(reader, descriptor, offset, length)?;
        let actual = crc32(&bytes);
        if actual != block.checksum {
            return Err(BucketError::DirectoryChecksumMismatch {
                expected: block.checksum,
                actual,
            });
        }
        let entries: Vec<&[u8]> = bytes.chunks_exact(BUCKET_DIRECTORY_ENTRY_BYTES).collect();
        let mut low = 0;
        let mut high = entries.len();
        while low < high {
            let middle = low + (high - low) / 2;
            match entry_key(entries[middle])?.cmp(&key) {
                std::cmp::Ordering::Less => low = middle + 1,
                std::cmp::Ordering::Greater => high = middle,
                std::cmp::Ordering::Equal => {
                    let entry = entries[middle];
                    return Ok(Some(BucketLocation {
                        key,
                        section: descriptor,
                        member_offset: read_u64_at(entry, 16)?,
                        member_count: read_u32_at(entry, 24)?,
                        member_checksum: read_u32_at(entry, 28)?,
                    }));
                }
            }
        }
        Ok(None)
    }
}

fn entry_key(entry: &[u8]) -> Result<BucketKey, BucketError> {
    Ok(BucketKey {
        band: read_u32_at(entry, 0)?,
        hash: read_u64_at(entry, 8)?,
    })
}

fn read_u16_at(bytes: &[u8], offset: usize) -> Result<u16, BucketError> {
    let end = offset.checked_add(2).ok_or(BucketError::LengthOverflow)?;
    let raw: [u8; 2] = bytes
        .get(offset..end)
        .ok_or(BucketError::Invalid {
            reason: "fixed-width u16 field is truncated",
        })?
        .try_into()
        .map_err(|_| BucketError::Invalid {
            reason: "fixed-width u16 field is truncated",
        })?;
    Ok(u16::from_le_bytes(raw))
}

fn read_u32_at(bytes: &[u8], offset: usize) -> Result<u32, BucketError> {
    let end = offset.checked_add(4).ok_or(BucketError::LengthOverflow)?;
    let raw: [u8; 4] = bytes
        .get(offset..end)
        .ok_or(BucketError::Invalid {
            reason: "fixed-width u32 field is truncated",
        })?
        .try_into()
        .map_err(|_| BucketError::Invalid {
            reason: "fixed-width u32 field is truncated",
        })?;
    Ok(u32::from_le_bytes(raw))
}

fn read_u64_at(bytes: &[u8], offset: usize) -> Result<u64, BucketError> {
    let end = offset
        .checked_add(U64_BYTES)
        .ok_or(BucketError::LengthOverflow)?;
    let raw = bytes.get(offset..end).ok_or(BucketError::Invalid {
        reason: "fixed-width u64 field is truncated",
    })?;
    read_u64(raw)
}

fn read_u64(bytes: &[u8]) -> Result<u64, BucketError> {
    let raw: [u8; U64_BYTES] = bytes.try_into().map_err(|_| BucketError::Invalid {
        reason: "fixed-width u64 field is truncated",
    })?;
    Ok(u64::from_le_bytes(raw))
}

/// Directory bytes a [`BucketRangeCursor`] reads at once, across blocks.
const RANGE_DIRECTORY_BATCH_BYTES: usize = 256 * 1024;

impl BucketDirectory {
    /// Return up to `parts - 1` ascending bucket keys that split this
    /// directory's buckets into about `parts` ranges of similar bucket counts.
    ///
    /// Every key is the first key of a directory block, so ranges that start
    /// at these keys begin at block boundaries in this directory.
    #[must_use]
    pub fn split_keys(&self, parts: usize) -> Vec<BucketKey> {
        if parts < 2 || self.blocks.len() < 2 {
            return Vec::new();
        }
        let step = self.blocks.len().div_ceil(parts).max(1);
        self.blocks
            .iter()
            .step_by(step)
            .skip(1)
            .map(|block| block.first_key)
            .collect()
    }

    /// Iterate the buckets whose keys are at least `start` (or every bucket
    /// when `None`) and below `end` (or to the last bucket when `None`).
    ///
    /// The cursor reads directory blocks in batches, verifies each block's
    /// checksum against this index and each member list's checksum against
    /// its directory entry, exactly like [`Self::find`] and
    /// [`read_bucket_members`]. Cursors over disjoint ranges can run on
    /// separate threads with separate readers.
    #[must_use]
    pub fn range_cursor(
        &self,
        start: Option<BucketKey>,
        end: Option<BucketKey>,
    ) -> BucketRangeCursor<'_> {
        // The block holding `start`: the last block whose first key is not
        // above it. Earlier entries of that block are skipped.
        let block = start.map_or(0, |start| {
            self.blocks
                .partition_point(|block| block.first_key <= start)
                .saturating_sub(1)
        });
        BucketRangeCursor {
            directory: self,
            next_block: block,
            start,
            end,
            entries: Vec::new(),
            entry: 0,
            members: Vec::new(),
            members_offset: 0,
            finished: false,
        }
    }
}

/// Buckets of one key range of a [`BucketDirectory`], in key order.
#[derive(Debug)]
pub struct BucketRangeCursor<'a> {
    directory: &'a BucketDirectory,
    next_block: usize,
    start: Option<BucketKey>,
    end: Option<BucketKey>,
    /// Verified directory entries of the current batch.
    entries: Vec<u8>,
    entry: usize,
    /// Member bytes of the current batch, starting at `members_offset`.
    members: Vec<u8>,
    members_offset: u64,
    finished: bool,
}

impl BucketRangeCursor<'_> {
    /// Return the next bucket in the range and replace `members` with its
    /// member keys, or `None` after the range.
    pub fn next_bucket<R: Read + Seek>(
        &mut self,
        layout: &FileLayout,
        reader: &mut R,
        members: &mut Vec<u64>,
    ) -> Result<Option<BucketKey>, BucketError> {
        loop {
            if self.finished {
                return Ok(None);
            }
            if self.entry * BUCKET_DIRECTORY_ENTRY_BYTES >= self.entries.len() {
                if !self.load_batch(layout, reader)? {
                    self.finished = true;
                    return Ok(None);
                }
                continue;
            }
            let at = self.entry * BUCKET_DIRECTORY_ENTRY_BYTES;
            let entry = &self.entries[at..at + BUCKET_DIRECTORY_ENTRY_BYTES];
            self.entry += 1;
            let key = entry_key(entry)?;
            if self.start.is_some_and(|start| key < start) {
                continue;
            }
            if self.end.is_some_and(|end| key >= end) {
                self.finished = true;
                return Ok(None);
            }
            let offset = read_u64_at(entry, 16)?;
            let count = read_u32_at(entry, 24)?;
            let checksum = read_u32_at(entry, 28)?;
            let length = usize::try_from(count)
                .map_err(|_| BucketError::LengthOverflow)?
                .checked_mul(U64_BYTES)
                .ok_or(BucketError::LengthOverflow)?;
            let first = usize::try_from(offset.checked_sub(self.members_offset).ok_or(
                BucketError::Invalid {
                    reason: "bucket member range precedes its batch",
                },
            )?)
            .map_err(|_| BucketError::LengthOverflow)?;
            let bytes = first
                .checked_add(length)
                .and_then(|last| self.members.get(first..last))
                .ok_or(BucketError::Invalid {
                    reason: "bucket member range extends past its batch",
                })?;
            let actual = crc32(bytes);
            if actual != checksum {
                return Err(BucketError::MemberChecksumMismatch {
                    expected: checksum,
                    actual,
                });
            }
            members.clear();
            for chunk in bytes.chunks_exact(U64_BYTES) {
                members.push(read_u64(chunk)?);
            }
            return Ok(Some(key));
        }
    }

    /// Read the next batch of contiguous blocks of one section and their
    /// member bytes. Return `false` when no block is left in the range.
    fn load_batch<R: Read + Seek>(
        &mut self,
        layout: &FileLayout,
        reader: &mut R,
    ) -> Result<bool, BucketError> {
        let blocks = &self.directory.blocks;
        let Some(first) = blocks.get(self.next_block).copied() else {
            return Ok(false);
        };
        if self.end.is_some_and(|end| first.first_key >= end) {
            return Ok(false);
        }
        // Extend the batch over following blocks of the same section.
        let mut last = self.next_block;
        let mut entries =
            usize::try_from(first.entries).map_err(|_| BucketError::LengthOverflow)?;
        while let Some(next) = blocks.get(last + 1) {
            let previous = blocks[last];
            let next_entries =
                usize::try_from(next.entries).map_err(|_| BucketError::LengthOverflow)?;
            if next.section != first.section
                || next.first_entry != previous.first_entry + previous.entries
                || (entries + next_entries) * BUCKET_DIRECTORY_ENTRY_BYTES
                    > RANGE_DIRECTORY_BATCH_BYTES
                || self.end.is_some_and(|end| next.first_key >= end)
            {
                break;
            }
            entries += next_entries;
            last += 1;
        }
        let descriptor = *self
            .directory
            .sections
            .get(usize::try_from(first.section).map_err(|_| BucketError::LengthOverflow)?)
            .ok_or(BucketError::Invalid {
                reason: "bucket directory block references a missing section",
            })?;
        let offset = u64::from(first.first_entry)
            .checked_mul(BUCKET_DIRECTORY_ENTRY_BYTES as u64)
            .and_then(|bytes| bytes.checked_add(BUCKET_SEGMENT_HEADER_BYTES_U64))
            .ok_or(BucketError::LengthOverflow)?;
        let length = entries
            .checked_mul(BUCKET_DIRECTORY_ENTRY_BYTES)
            .ok_or(BucketError::LengthOverflow)?;
        let bytes = layout.read_section_range(reader, descriptor, offset, length)?;
        let mut at = 0;
        for block in &blocks[self.next_block..=last] {
            let size = usize::try_from(block.entries).map_err(|_| BucketError::LengthOverflow)?
                * BUCKET_DIRECTORY_ENTRY_BYTES;
            let actual = crc32(&bytes[at..at + size]);
            if actual != block.checksum {
                return Err(BucketError::DirectoryChecksumMismatch {
                    expected: block.checksum,
                    actual,
                });
            }
            at += size;
        }
        // Member lists of consecutive entries are contiguous.
        let first_entry = &bytes[..BUCKET_DIRECTORY_ENTRY_BYTES];
        let last_entry = &bytes[length - BUCKET_DIRECTORY_ENTRY_BYTES..];
        let members_start = read_u64_at(first_entry, 16)?;
        let members_end = read_u64_at(last_entry, 16)?
            .checked_add(
                u64::from(read_u32_at(last_entry, 24)?)
                    .checked_mul(U64_BYTES_U64)
                    .ok_or(BucketError::LengthOverflow)?,
            )
            .ok_or(BucketError::LengthOverflow)?;
        let member_length = usize::try_from(members_end.checked_sub(members_start).ok_or(
            BucketError::Invalid {
                reason: "bucket member ranges are out of order",
            },
        )?)
        .map_err(|_| BucketError::LengthOverflow)?;
        self.members =
            layout.read_section_range(reader, descriptor, members_start, member_length)?;
        self.members_offset = members_start;
        self.entries = bytes;
        self.entry = 0;
        self.next_block = last + 1;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::{
        decode_bucket_segment, encode_bucket_segment, read_bucket_members,
        validate_global_bucket_order, BucketDirectory, BucketError, BucketKey, BucketRecord,
    };
    use crate::{
        Algorithm, CodecId, FileLayout, IndexFile, IndexMetadata, Section, SectionKind,
        SignatureScheme,
    };

    fn metadata() -> IndexMetadata {
        IndexMetadata::new(
            Algorithm::MinHashLsh,
            SignatureScheme::PariAffine32V1,
            CodecId::U64,
            128,
            7,
            0.8,
            32,
            4,
            0,
        )
        .expect("valid metadata")
    }

    #[test]
    fn segment_round_trip_keeps_directory_lazy() {
        let first = [1_u64, 2, 3];
        let second = [7_u64, 9];
        let payload = encode_bucket_segment(&[
            BucketRecord::new(BucketKey::new(0, 11), &first),
            BucketRecord::new(BucketKey::new(1, 22), &second),
        ])
        .expect("encode");
        let bytes = IndexFile::new(
            metadata(),
            vec![Section::new(SectionKind::Buckets, true, payload).expect("section")],
        )
        .expect("file")
        .encode()
        .expect("container");
        let mut cursor = Cursor::new(bytes);
        let layout = FileLayout::read_from(&mut cursor).expect("layout");
        let section = layout.section(SectionKind::Buckets).expect("buckets");
        let locations = decode_bucket_segment(&layout, &mut cursor, section, 32).expect("decode");
        validate_global_bucket_order(&locations).expect("global order");
        assert_eq!(locations.len(), 2);
        assert_eq!(
            read_bucket_members(&layout, &mut cursor, locations[0]).expect("members"),
            first
        );
        assert_eq!(
            read_bucket_members(&layout, &mut cursor, locations[1]).expect("members"),
            second
        );
    }

    /// A file whose buckets span several sections and many directory blocks
    /// in every band.
    fn many_bucket_file() -> (Vec<u8>, Vec<(BucketKey, Vec<u64>)>) {
        let mut buckets = Vec::new();
        for band in 0..32_u32 {
            for hash in 0..150_u64 {
                let members: Vec<u64> = (0..=(hash % 3)).map(|offset| hash * 10 + offset).collect();
                buckets.push((
                    BucketKey::new(band, hash * 1_000 + u64::from(band)),
                    members,
                ));
            }
        }
        let sections = buckets
            .chunks(700)
            .map(|chunk| {
                let records: Vec<_> = chunk
                    .iter()
                    .map(|(key, members)| BucketRecord::new(*key, members))
                    .collect();
                Section::new(
                    SectionKind::Buckets,
                    true,
                    encode_bucket_segment(&records).expect("encode"),
                )
                .expect("section")
            })
            .collect();
        let bytes = IndexFile::new(metadata(), sections)
            .expect("file")
            .encode()
            .expect("container");
        (bytes, buckets)
    }

    #[test]
    fn parallel_directory_reads_match_sequential_reads() {
        let (bytes, _) = many_bucket_file();
        let read = |bytes: &[u8], workers: usize| {
            let mut reader = Cursor::new(bytes.to_vec());
            let layout = FileLayout::read_from(&mut reader).expect("layout");
            BucketDirectory::read_with_workers(&layout, &mut reader, layout.sections(), 32, workers)
        };
        let sequential = read(&bytes, 0).expect("sequential");
        assert!(sequential.sections.len() > 4);
        for workers in [2, 3, 4] {
            assert_eq!(read(&bytes, workers).expect("parallel"), sequential);
        }

        // Damage that sequential and parallel reads must report alike: a
        // directory checksum in a middle section, a reserved header byte in a
        // later section, and two sections swapped so global order breaks at a
        // boundary.
        let layout = FileLayout::read_from(&mut Cursor::new(bytes.clone())).expect("layout");
        let offset = |section: usize| {
            usize::try_from(layout.sections()[section].payload_offset()).expect("offset")
        };
        let header = super::BUCKET_SEGMENT_HEADER_BYTES;
        let mut damaged = Vec::new();
        let mut directory = bytes.clone();
        directory[offset(3) + header + 40] ^= 0xFF;
        damaged.push(directory);
        let mut later = bytes.clone();
        later[offset(5) + 36] ^= 0x01;
        damaged.push(later);
        // Two sections swapped, rebuilt as a valid container, so only the
        // global bucket order breaks, at a section boundary.
        let file = IndexFile::decode(&bytes).expect("file");
        let mut sections = file.sections().to_vec();
        sections.swap(2, 4);
        let swapped = IndexFile::new(file.metadata().clone(), sections)
            .expect("file")
            .encode()
            .expect("container");
        damaged.push(swapped);
        let kinds = [
            "DirectoryChecksumMismatch",
            "bucket segment reserved bytes must be zero",
            "bucket directory contains duplicate or unsorted bucket keys",
        ];
        for (case, bytes) in damaged.iter().enumerate() {
            let expected = format!("{:?}", read(bytes, 0).expect_err("sequential error"));
            assert!(expected.contains(kinds[case]), "case {case}: {expected}");
            for workers in [2, 4] {
                let actual = format!("{:?}", read(bytes, workers).expect_err("parallel error"));
                assert_eq!(actual, expected, "case {case} with {workers} workers");
            }
        }
    }

    #[test]
    fn range_cursors_cover_every_bucket_exactly_once() {
        let (bytes, expected) = many_bucket_file();
        let mut reader = Cursor::new(bytes);
        let layout = FileLayout::read_from(&mut reader).expect("layout");
        let directory =
            BucketDirectory::read(&layout, &mut reader, layout.sections(), 32).expect("directory");
        for parts in [1, 2, 7, 64, 10_000] {
            let splits = directory.split_keys(parts);
            assert!(splits.windows(2).all(|pair| pair[0] < pair[1]));
            let mut bounds = vec![None];
            bounds.extend(splits.iter().copied().map(Some));
            bounds.push(None);
            let mut actual = Vec::new();
            let mut members = Vec::new();
            for range in bounds.windows(2) {
                let mut cursor = directory.range_cursor(range[0], range[1]);
                while let Some(key) = cursor
                    .next_bucket(&layout, &mut reader, &mut members)
                    .expect("bucket")
                {
                    actual.push((key, members.clone()));
                }
            }
            assert_eq!(actual, expected, "{parts} parts");
        }
        // Ranges that start and end inside directory blocks.
        let start = expected[123].0;
        let end = expected[2_345].0;
        let mut cursor = directory.range_cursor(Some(start), Some(end));
        let mut members = Vec::new();
        let mut actual = Vec::new();
        while let Some(key) = cursor
            .next_bucket(&layout, &mut reader, &mut members)
            .expect("bucket")
        {
            actual.push((key, members.clone()));
        }
        assert_eq!(actual, expected[123..2_345]);
    }

    #[test]
    fn range_cursors_detect_damaged_members_and_directories() {
        let (bytes, _) = many_bucket_file();
        let mut reader = Cursor::new(bytes.clone());
        let layout = FileLayout::read_from(&mut reader).expect("layout");
        let directory =
            BucketDirectory::read(&layout, &mut reader, layout.sections(), 32).expect("directory");
        let section = layout.sections()[0];
        let payload = usize::try_from(section.payload_offset()).expect("offset");
        let directory_end = payload + super::BUCKET_SEGMENT_HEADER_BYTES + 700 * 32;
        for (at, member) in [(directory_end + 3, true), (payload + 50, false)] {
            let mut damaged = bytes.clone();
            damaged[at] ^= 0xFF;
            let mut reader = Cursor::new(damaged);
            let mut cursor = directory.range_cursor(None, None);
            let mut members = Vec::new();
            let error = loop {
                match cursor.next_bucket(&layout, &mut reader, &mut members) {
                    Ok(Some(_)) => {}
                    Ok(None) => panic!("damage at {at} was not detected"),
                    Err(error) => break error,
                }
            };
            if member {
                assert!(matches!(error, BucketError::MemberChecksumMismatch { .. }));
            } else {
                assert!(matches!(
                    error,
                    BucketError::DirectoryChecksumMismatch { .. }
                ));
            }
        }
    }

    #[test]
    fn member_corruption_is_detected_on_read() {
        let members = [5_u64, 6];
        let payload = encode_bucket_segment(&[BucketRecord::new(BucketKey::new(0, 11), &members)])
            .expect("encode");
        let mut bytes = IndexFile::new(
            metadata(),
            vec![Section::new(SectionKind::Buckets, true, payload).expect("section")],
        )
        .expect("file")
        .encode()
        .expect("container");
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        let mut cursor = Cursor::new(bytes);
        let layout = FileLayout::read_from(&mut cursor).expect("layout");
        let section = layout.section(SectionKind::Buckets).expect("buckets");
        let locations = decode_bucket_segment(&layout, &mut cursor, section, 32).expect("decode");
        assert!(read_bucket_members(&layout, &mut cursor, locations[0]).is_err());
    }
}
