use std::{
    collections::BTreeMap,
    io::Write,
    mem::size_of,
    ops::{Range, RangeInclusive},
};

use pari_format::{
    bucket_record_size, encode_bucket_segment, BucketKey, BucketRecord, ContainerWriter,
    FormatError, SectionKind, BUCKET_SEGMENT_HEADER_BYTES, MAX_SECTION_BYTES,
};
use rayon::slice::ParallelSliceMut;

use crate::StoreError;

type Membership = (BucketKey, u64);

/// Maximum scratch space for sorting memberships, regardless of index size.
pub(super) const SORT_RECORD_LIMIT: usize = 128 * 1024 * 1024 / size_of::<Membership>();

pub(super) fn write_bucket_sections<W: Write>(
    writer: &mut ContainerWriter<W>,
    key_hashes: &BTreeMap<u64, Vec<u64>>,
    bands: usize,
    target_bytes: usize,
    sort_limit: usize,
) -> Result<(), StoreError> {
    let mut output = BucketSegments {
        writer,
        target_bytes,
        size: BUCKET_SEGMENT_HEADER_BYTES,
        members: Vec::new(),
        buckets: Vec::new(),
    };
    if key_hashes.is_empty() {
        return output.flush();
    }

    let rows = key_hashes.len();
    let bands_per_batch = (sort_limit / rows).clamp(1, bands);
    let mut scratch = Vec::with_capacity(rows.saturating_mul(bands_per_batch).min(sort_limit));
    if rows <= sort_limit {
        for start in (0..bands).step_by(bands_per_batch) {
            let end = start.saturating_add(bands_per_batch).min(bands);
            collect_bands(key_hashes, start..end, &mut scratch)?;
            output.sort_and_write(&mut scratch)?;
        }
    } else {
        for band in 0..bands {
            let partition = BandPartition {
                key_hashes,
                band,
                sort_limit,
            };
            partition.write_range(0..=u64::MAX, &mut scratch, &mut output)?;
        }
    }
    output.flush()
}

fn collect_bands(
    key_hashes: &BTreeMap<u64, Vec<u64>>,
    range: Range<usize>,
    scratch: &mut Vec<Membership>,
) -> Result<(), StoreError> {
    scratch.clear();
    for (&key, hashes) in key_hashes {
        for band in range.clone() {
            let bucket = bucket_key(band, hashes[band])?;
            scratch.push((bucket, key));
        }
    }
    Ok(())
}

fn bucket_key(band: usize, hash: u64) -> Result<BucketKey, StoreError> {
    let band = u32::try_from(band).map_err(|_| StoreError::LengthOverflow)?;
    Ok(BucketKey::new(band, hash))
}

struct BandPartition<'a> {
    key_hashes: &'a BTreeMap<u64, Vec<u64>>,
    band: usize,
    sort_limit: usize,
}

impl BandPartition<'_> {
    /// Split an oversized band by its actual hash range. A split never
    /// divides a bucket, and visiting the lower half first preserves order.
    fn write_range<W: Write>(
        &self,
        range: RangeInclusive<u64>,
        scratch: &mut Vec<Membership>,
        output: &mut BucketSegments<'_, W>,
    ) -> Result<(), StoreError> {
        scratch.clear();
        let mut count = 0;
        let mut min = u64::MAX;
        let mut max = 0;
        for (&key, hashes) in self.key_hashes {
            let hash = hashes[self.band];
            if range.contains(&hash) {
                if count < self.sort_limit {
                    scratch.push((bucket_key(self.band, hash)?, key));
                }
                count += 1;
                min = min.min(hash);
                max = max.max(hash);
            }
        }
        if count <= self.sort_limit {
            return output.sort_and_write(scratch);
        }
        if min == max {
            // Keys from the ordered catalog are already sorted for this bucket.
            let keys = self
                .key_hashes
                .iter()
                .filter_map(|(&key, hashes)| (hashes[self.band] == min).then_some(key));
            return output.push(bucket_key(self.band, min)?, count, keys);
        }

        let middle = min + (max - min) / 2;
        self.write_range(min..=middle, scratch, output)?;
        self.write_range(middle + 1..=max, scratch, output)
    }
}

struct BucketSegments<'a, W: Write> {
    writer: &'a mut ContainerWriter<W>,
    target_bytes: usize,
    size: usize,
    members: Vec<u64>,
    buckets: Vec<(BucketKey, Range<usize>)>,
}

impl<W: Write> BucketSegments<'_, W> {
    fn sort_and_write(&mut self, memberships: &mut [Membership]) -> Result<(), StoreError> {
        memberships.par_sort_unstable();
        let mut rest = &*memberships;
        while let Some(&(bucket, _)) = rest.first() {
            let count = rest.iter().take_while(|&&(key, _)| key == bucket).count();
            let (group, remaining) = rest.split_at(count);
            self.push(bucket, count, group.iter().map(|&(_, key)| key))?;
            rest = remaining;
        }
        Ok(())
    }

    fn push(
        &mut self,
        bucket: BucketKey,
        count: usize,
        members: impl Iterator<Item = u64>,
    ) -> Result<(), StoreError> {
        let contribution = bucket_record_size(count)?;
        let single_size = BUCKET_SEGMENT_HEADER_BYTES
            .checked_add(contribution)
            .ok_or(StoreError::LengthOverflow)?;
        let length = u64::try_from(single_size).map_err(|_| StoreError::LengthOverflow)?;
        if length > MAX_SECTION_BYTES {
            return Err(FormatError::SectionTooLarge { length }.into());
        }

        if !self.buckets.is_empty() && self.size + contribution > self.target_bytes {
            self.flush()?;
        }
        let start = self.members.len();
        if single_size > self.target_bytes {
            self.members.reserve_exact(count);
        }
        self.members.extend(members);
        self.buckets.push((bucket, start..self.members.len()));
        self.size += contribution;
        Ok(())
    }

    fn flush(&mut self) -> Result<(), StoreError> {
        let records: Vec<_> = self
            .buckets
            .iter()
            .map(|(bucket, range)| BucketRecord::new(*bucket, &self.members[range.clone()]))
            .collect();
        let payload = encode_bucket_segment(&records)?;
        self.writer
            .write_section(SectionKind::Buckets, true, &payload)?;
        if self.size > self.target_bytes {
            self.members = Vec::new();
        } else {
            self.members.clear();
        }
        self.size = BUCKET_SEGMENT_HEADER_BYTES;
        self.buckets.clear();
        Ok(())
    }
}
