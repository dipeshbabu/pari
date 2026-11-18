//! Appended commit generations: commit points, interrupted commits, corruption.

use std::io::{Cursor, Seek, SeekFrom, Write};

use pari_format::{
    Algorithm, CodecId, ContainerWriteError, ContainerWriter, FileLayout, FormatError,
    IndexMetadata, LayoutError, SectionKind, SignatureScheme, COMMIT_TRAILER_BYTES,
    DELTA_HEADER_BYTES,
};

fn metadata(seed: u64) -> IndexMetadata {
    IndexMetadata::new(
        Algorithm::MinHashLsh,
        SignatureScheme::PariAffine32V1,
        CodecId::U64,
        128,
        seed,
        0.8,
        32,
        4,
        0,
    )
    .expect("metadata")
}

fn base(seed: u64) -> Vec<u8> {
    ContainerWriter::write_seekable(Cursor::new(Vec::new()), &metadata(seed), |writer| {
        writer.write_section(SectionKind::Keys, true, &[1, 2, 3, 4])?;
        writer.write_section(SectionKind::Signatures, false, &[9; 40])?;
        Ok::<_, ContainerWriteError>(())
    })
    .expect("base")
    .into_inner()
}

/// Append one record with the given payloads; commit it when `commit` is set.
fn append(bytes: Vec<u8>, payloads: &[(SectionKind, bool, Vec<u8>)], commit: bool) -> Vec<u8> {
    let mut cursor = Cursor::new(bytes);
    let layout = FileLayout::read_with_generations(&mut cursor).expect("layout");
    let next = layout.next_delta();
    let mut bytes = cursor.into_inner();
    bytes.truncate(usize::try_from(next.offset()).expect("offset"));
    let mut cursor = Cursor::new(bytes);
    cursor.seek(SeekFrom::End(0)).expect("seek");
    let (mut cursor, record) = ContainerWriter::write_delta(cursor, next, |writer| {
        for (kind, required, payload) in payloads {
            writer.write_section(*kind, *required, payload)?;
        }
        Ok::<_, ContainerWriteError>(())
    })
    .expect("delta");
    assert_eq!(record.generation(), next.generation());
    if commit {
        assert_eq!(
            cursor.stream_position().expect("position"),
            record.trailer_offset()
        );
        cursor.write_all(&record.trailer()).expect("trailer");
    }
    cursor.into_inner()
}

fn two_generations() -> Vec<u8> {
    let bytes = append(
        base(42),
        &[
            (SectionKind::Keys, true, vec![5; 16]),
            (SectionKind::Tombstones, true, vec![6; 8]),
        ],
        true,
    );
    append(
        bytes,
        &[
            (SectionKind::Keys, true, vec![7; 24]),
            (SectionKind::Signatures, false, vec![8; 12]),
        ],
        true,
    )
}

fn read(bytes: &[u8]) -> Result<FileLayout, LayoutError> {
    FileLayout::read_with_generations(&mut Cursor::new(bytes))
}

/// Read every section of every generation, verifying payload checksums.
fn read_all_sections(bytes: &[u8]) -> Result<FileLayout, LayoutError> {
    let layout = read(bytes)?;
    let mut cursor = Cursor::new(bytes);
    for delta in layout.deltas() {
        for section in delta.sections() {
            layout.read_section(&mut cursor, *section)?;
        }
    }
    Ok(layout)
}

#[test]
fn committed_generations_are_read_in_order() {
    let bytes = two_generations();
    let layout = read(&bytes).expect("layout");
    let deltas = layout.deltas();
    assert_eq!(deltas.len(), 2);
    assert_eq!(deltas[0].generation(), 1);
    assert_eq!(deltas[1].generation(), 2);
    assert_eq!(deltas[0].offset(), layout.base_length());
    assert_eq!(deltas[1].offset(), deltas[0].end());
    assert_eq!(layout.file_length(), deltas[1].end());
    assert_eq!(
        layout.file_length(),
        u64::try_from(bytes.len()).expect("len")
    );
    assert_eq!(layout.uncommitted_bytes(), 0);
    assert_eq!(layout.next_delta().generation(), 3);
    assert_eq!(layout.next_delta().offset(), layout.file_length());

    let kinds: Vec<_> = deltas[0].sections().iter().map(|s| s.kind()).collect();
    assert_eq!(kinds, [SectionKind::Keys, SectionKind::Tombstones]);
    let mut cursor = Cursor::new(&bytes);
    assert_eq!(
        layout
            .read_section(&mut cursor, deltas[1].sections()[0])
            .expect("payload"),
        vec![7; 24]
    );
    assert!(!deltas[1].sections()[1].required());
    assert_eq!(layout.sections().len(), 2, "base sections are unchanged");
}

#[test]
fn base_only_readers_refuse_appended_generations() {
    let bytes = two_generations();
    assert!(matches!(
        FileLayout::read_from(&mut Cursor::new(&bytes)),
        Err(LayoutError::Format(FormatError::AppendedGenerations))
    ));
    let uncommitted = append(base(42), &[(SectionKind::Keys, true, vec![1])], false);
    assert!(matches!(
        FileLayout::read_from(&mut Cursor::new(&uncommitted)),
        Err(LayoutError::Format(FormatError::AppendedGenerations))
    ));
    let plain = base(42);
    let layout = FileLayout::read_from(&mut Cursor::new(&plain)).expect("plain base");
    assert!(layout.deltas().is_empty());
    assert_eq!(layout, read(&plain).expect("generation-aware read"));
}

#[test]
fn interrupted_commit_is_ignored_and_replaced() {
    let committed = append(base(42), &[(SectionKind::Keys, true, vec![5; 16])], true);
    let interrupted = append(
        committed.clone(),
        &[(SectionKind::Keys, true, vec![1; 99])],
        false,
    );
    let layout = read(&interrupted).expect("layout");
    assert_eq!(layout.deltas().len(), 1);
    assert_eq!(
        layout.file_length(),
        u64::try_from(committed.len()).expect("len")
    );
    assert_eq!(
        layout.uncommitted_bytes(),
        u64::try_from(interrupted.len() - committed.len()).expect("len")
    );

    // The next commit overwrites the interrupted record.
    let replaced = append(interrupted, &[(SectionKind::Keys, true, vec![2; 8])], true);
    let layout = read_all_sections(&replaced).expect("replaced");
    assert_eq!(layout.deltas().len(), 2);
    assert_eq!(layout.uncommitted_bytes(), 0);
}

#[test]
fn every_truncation_keeps_exactly_the_complete_generations() {
    let bytes = two_generations();
    let layout = read(&bytes).expect("layout");
    let base_length = usize::try_from(layout.base_length()).expect("len");
    let ends: Vec<_> = layout
        .deltas()
        .iter()
        .map(|delta| usize::try_from(delta.end()).expect("end"))
        .collect();
    for end in base_length..=bytes.len() {
        let truncated = read_all_sections(&bytes[..end])
            .unwrap_or_else(|error| panic!("prefix {end} rejected: {error}"));
        let complete = ends.iter().filter(|delta_end| **delta_end <= end).count();
        assert_eq!(truncated.deltas().len(), complete, "prefix {end}");
        assert_eq!(
            truncated.file_length() + truncated.uncommitted_bytes(),
            u64::try_from(end).expect("len")
        );
    }
}

#[test]
fn every_corrupted_byte_is_detected_or_drops_only_the_last_trailer() {
    let bytes = two_generations();
    let layout = read(&bytes).expect("layout");
    let base_length = usize::try_from(layout.base_length()).expect("len");
    let last_trailer = bytes.len() - COMMIT_TRAILER_BYTES;
    for index in base_length..bytes.len() {
        let mut corrupt = bytes.clone();
        corrupt[index] ^= 0x40;
        match read_all_sections(&corrupt) {
            Err(_) => {}
            Ok(layout) => {
                // Damage to the final trailer is indistinguishable from an
                // interrupted commit, so that generation is not committed.
                assert!(
                    index >= last_trailer,
                    "corruption at {index} went unnoticed"
                );
                assert_eq!(layout.deltas().len(), 1, "byte {index}");
            }
        }
    }
}

#[test]
fn records_are_bound_to_their_base_and_predecessor() {
    let bytes = two_generations();
    let layout = read(&bytes).expect("layout");
    let base_length = usize::try_from(layout.base_length()).expect("len");
    let first_end = usize::try_from(layout.deltas()[0].end()).expect("end");

    // A record committed on a different base is rejected.
    let mut spliced = base(7);
    spliced.extend_from_slice(&bytes[base_length..first_end]);
    assert!(matches!(
        read(&spliced),
        Err(LayoutError::Format(FormatError::InvalidGeneration {
            generation: 1,
            ..
        }))
    ));

    // Generation 2 cannot follow the base directly.
    let mut skipped = bytes[..base_length].to_vec();
    skipped.extend_from_slice(&bytes[first_end..]);
    assert!(matches!(
        read(&skipped),
        Err(LayoutError::Format(FormatError::InvalidGeneration { .. }))
    ));
}

#[test]
fn refresh_reads_only_new_generations() {
    let first = append(base(42), &[(SectionKind::Keys, true, vec![5; 16])], true);
    let mut layout = read(&first).expect("layout");
    let second = append(first, &[(SectionKind::Keys, true, vec![6; 8])], true);
    assert_eq!(
        layout
            .refresh_generations(&mut Cursor::new(&second))
            .expect("refresh"),
        1
    );
    assert_eq!(layout, read(&second).expect("fresh read"));
    assert_eq!(
        layout
            .refresh_generations(&mut Cursor::new(&second))
            .expect("no change"),
        0
    );
    assert!(layout
        .refresh_generations(&mut Cursor::new(&second[..second.len() - 1]))
        .is_err());
}

#[test]
fn delta_records_must_start_at_the_committed_end() {
    let bytes = base(42);
    let layout = read(&bytes).expect("layout");
    let mut cursor = Cursor::new(bytes);
    cursor.seek(SeekFrom::Start(3)).expect("seek");
    let result = ContainerWriter::write_delta(cursor, layout.next_delta(), |_| {
        Ok::<_, ContainerWriteError>(())
    });
    assert!(matches!(result, Err(ContainerWriteError::Io(_))));
}

#[test]
fn empty_records_commit() {
    let bytes = append(base(42), &[], true);
    let layout = read(&bytes).expect("layout");
    assert_eq!(layout.deltas().len(), 1);
    assert!(layout.deltas()[0].sections().is_empty());
    assert_eq!(
        layout.file_length() - layout.base_length(),
        u64::try_from(DELTA_HEADER_BYTES + COMMIT_TRAILER_BYTES).expect("len")
    );
}
