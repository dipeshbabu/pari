//! Streaming writer for the version-1 container.
//!
//! [`ContainerWriter`] produces exactly the bytes of [`crate::IndexFile::encode`]
//! while writing each section as soon as it is ready, so a writer never has to
//! hold the complete container in memory.

use std::{
    error::Error,
    fmt,
    io::{self, Seek, SeekFrom, Write},
};

use crc32fast::hash as crc32;

use crate::{
    deltas::{encode_delta_header, record_digest, DELTA_HEADER_BYTES},
    format::{encode_header, MAX_SECTION_BYTES, MAX_SECTION_COUNT, SECTION_FLAG_REQUIRED},
    DeltaCommit, FormatError, IndexMetadata, NextDelta, SectionKind,
};

/// Errors from [`ContainerWriter`].
#[derive(Debug)]
pub enum ContainerWriteError {
    /// Writing to the underlying sink failed.
    Io(io::Error),
    /// A section or the section count violates the container format.
    Format(FormatError),
    /// The number of sections written differs from the declared count.
    SectionCountMismatch { declared: u32, written: u32 },
}

impl fmt::Display for ContainerWriteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "container write failed: {error}"),
            Self::Format(error) => error.fmt(formatter),
            Self::SectionCountMismatch { declared, written } => write!(
                formatter,
                "container declared {declared} sections but {written} were written"
            ),
        }
    }
}

impl Error for ContainerWriteError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Format(error) => Some(error),
            Self::SectionCountMismatch { .. } => None,
        }
    }
}

impl From<io::Error> for ContainerWriteError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<FormatError> for ContainerWriteError {
    fn from(error: FormatError) -> Self {
        Self::Format(error)
    }
}

/// Write a version-1 container one section at a time.
///
/// Supply the section count up front with [`Self::new`], or use
/// [`Self::write_seekable`] to fill in the count after writing the sections.
#[derive(Debug)]
pub struct ContainerWriter<W: Write> {
    writer: W,
    declared: Option<u32>,
    written: u32,
    /// Section frames written so far, kept only for a delta record's digest.
    frames: Option<Vec<u8>>,
}

impl<W: Write> ContainerWriter<W> {
    /// Write the header for `section_count` sections.
    pub fn new(
        mut writer: W,
        metadata: &IndexMetadata,
        section_count: usize,
    ) -> Result<Self, ContainerWriteError> {
        if section_count > MAX_SECTION_COUNT {
            return Err(FormatError::TooManySections {
                count: section_count,
                max: MAX_SECTION_COUNT,
            }
            .into());
        }
        let declared = u32::try_from(section_count).map_err(|_| FormatError::LengthOverflow)?;
        writer.write_all(&encode_header(metadata, declared))?;
        Ok(Self {
            writer,
            declared: Some(declared),
            written: 0,
            frames: None,
        })
    }

    /// Frame and write one section payload.
    pub fn write_section(
        &mut self,
        kind: SectionKind,
        required: bool,
        payload: &[u8],
    ) -> Result<(), ContainerWriteError> {
        if let Some(declared) = self.declared {
            if self.written >= declared {
                return Err(ContainerWriteError::SectionCountMismatch {
                    declared,
                    written: self.written + 1,
                });
            }
        } else {
            let count =
                usize::try_from(self.written + 1).map_err(|_| FormatError::LengthOverflow)?;
            if count > MAX_SECTION_COUNT {
                return Err(FormatError::TooManySections {
                    count,
                    max: MAX_SECTION_COUNT,
                }
                .into());
            }
        }
        let length = u64::try_from(payload.len()).map_err(|_| FormatError::LengthOverflow)?;
        if length > MAX_SECTION_BYTES {
            return Err(FormatError::SectionTooLarge { length }.into());
        }
        let flags = if required { SECTION_FLAG_REQUIRED } else { 0 };
        let mut frame = [0_u8; 16];
        frame[0..2].copy_from_slice(&(kind as u16).to_le_bytes());
        frame[2..4].copy_from_slice(&flags.to_le_bytes());
        frame[4..12].copy_from_slice(&length.to_le_bytes());
        frame[12..16].copy_from_slice(&crc32(payload).to_le_bytes());
        self.writer.write_all(&frame)?;
        self.writer.write_all(payload)?;
        if let Some(frames) = &mut self.frames {
            frames.extend_from_slice(&frame);
        }
        self.written += 1;
        Ok(())
    }

    /// Verify the declared section count and return the underlying writer.
    pub fn finish(self) -> Result<W, ContainerWriteError> {
        if let Some(declared) = self.declared {
            if self.written != declared {
                return Err(ContainerWriteError::SectionCountMismatch {
                    declared,
                    written: self.written,
                });
            }
        }
        Ok(self.writer)
    }
}

impl<W: Write + Seek> ContainerWriter<W> {
    /// Write a container whose section count is only known after encoding.
    ///
    /// The sink must support overwriting bytes at the current position. On
    /// success it is positioned after the container. Use a staging file when
    /// publishing, since an error can leave a partial container.
    pub fn write_seekable<E>(
        mut writer: W,
        metadata: &IndexMetadata,
        write: impl FnOnce(&mut Self) -> Result<(), E>,
    ) -> Result<W, E>
    where
        E: From<ContainerWriteError>,
    {
        let start = writer.stream_position().map_err(ContainerWriteError::Io)?;
        writer
            .write_all(&encode_header(metadata, 0))
            .map_err(ContainerWriteError::Io)?;
        let mut container = Self {
            writer,
            declared: None,
            written: 0,
            frames: None,
        };
        write(&mut container)?;
        let mut writer = container.writer;
        let end = writer.stream_position().map_err(ContainerWriteError::Io)?;
        writer
            .seek(SeekFrom::Start(start))
            .map_err(ContainerWriteError::Io)?;
        writer
            .write_all(&encode_header(metadata, container.written))
            .map_err(ContainerWriteError::Io)?;
        writer
            .seek(SeekFrom::Start(end))
            .map_err(ContainerWriteError::Io)?;
        Ok(writer)
    }

    /// Write the header and sections of an appended delta record.
    ///
    /// The sink must be positioned at [`NextDelta::offset`]. On success it is
    /// positioned after the record, which is not committed yet: make it
    /// durable, then write [`DeltaCommit::trailer`] at
    /// [`DeltaCommit::trailer_offset`] and make that durable.
    pub fn write_delta<E>(
        mut writer: W,
        next: NextDelta,
        write: impl FnOnce(&mut Self) -> Result<(), E>,
    ) -> Result<(W, DeltaCommit), E>
    where
        E: From<ContainerWriteError>,
    {
        let start = writer.stream_position().map_err(ContainerWriteError::Io)?;
        if start != next.offset {
            return Err(ContainerWriteError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a delta record must start where the committed data ends",
            ))
            .into());
        }
        writer
            .write_all(&[0; DELTA_HEADER_BYTES])
            .map_err(ContainerWriteError::Io)?;
        let mut container = Self {
            writer,
            declared: None,
            written: 0,
            frames: Some(Vec::new()),
        };
        write(&mut container)?;
        let frames = container.frames.take().unwrap_or_default();
        let mut writer = container.writer;
        let end = writer.stream_position().map_err(ContainerWriteError::Io)?;
        let body_length = end
            .checked_sub(start)
            .and_then(|length| length.checked_sub(DELTA_HEADER_BYTES as u64))
            .ok_or(ContainerWriteError::Format(FormatError::LengthOverflow))?;
        let header = encode_delta_header(
            container.written,
            next.generation,
            body_length,
            next.previous_digest,
        );
        writer
            .seek(SeekFrom::Start(start))
            .map_err(ContainerWriteError::Io)?;
        writer.write_all(&header).map_err(ContainerWriteError::Io)?;
        writer
            .seek(SeekFrom::Start(end))
            .map_err(ContainerWriteError::Io)?;
        Ok((
            writer,
            DeltaCommit {
                generation: next.generation,
                offset: start,
                end,
                digest: record_digest(&header, &frames),
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Write};

    use super::{ContainerWriteError, ContainerWriter};
    use crate::{
        format::MAX_SECTION_COUNT, Algorithm, CodecId, FormatError, IndexFile, IndexMetadata,
        Section, SectionKind, SignatureScheme,
    };

    fn metadata() -> IndexMetadata {
        IndexMetadata::new(
            Algorithm::MinHashLsh,
            SignatureScheme::PariAffine32V1,
            CodecId::U64,
            128,
            42,
            0.8,
            32,
            4,
            0,
        )
        .expect("metadata")
    }

    #[test]
    fn streamed_bytes_match_in_memory_encoding() {
        let sections = vec![
            Section::new(SectionKind::Keys, true, vec![1, 2, 3]).expect("keys"),
            Section::new(SectionKind::BandHashes, true, vec![]).expect("hashes"),
            Section::new(SectionKind::Signatures, false, vec![9; 70]).expect("signatures"),
        ];
        let expected = IndexFile::new(metadata(), sections.clone())
            .expect("file")
            .encode()
            .expect("encode");
        let mut writer =
            ContainerWriter::new(Vec::new(), &metadata(), sections.len()).expect("writer");
        for section in &sections {
            writer
                .write_section(section.kind(), section.required(), section.payload())
                .expect("section");
        }
        assert_eq!(writer.finish().expect("finish"), expected);

        let prefix = b"prefix";
        let mut sink = Cursor::new(Vec::new());
        sink.write_all(prefix).expect("prefix");
        let mut sink = ContainerWriter::write_seekable(sink, &metadata(), |writer| {
            for section in &sections {
                writer.write_section(section.kind(), section.required(), section.payload())?;
            }
            Ok::<_, ContainerWriteError>(())
        })
        .expect("stream with unknown section count");
        sink.write_all(b"suffix").expect("suffix");
        assert_eq!(
            sink.into_inner(),
            [prefix, expected.as_slice(), b"suffix"].concat()
        );
    }

    #[test]
    fn section_count_is_enforced() {
        let mut writer = ContainerWriter::new(Vec::new(), &metadata(), 1).expect("writer");
        assert!(matches!(
            ContainerWriter::new(Vec::new(), &metadata(), 1)
                .expect("writer")
                .finish(),
            Err(ContainerWriteError::SectionCountMismatch {
                declared: 1,
                written: 0
            })
        ));
        writer
            .write_section(SectionKind::Keys, true, &[])
            .expect("first");
        assert!(matches!(
            writer.write_section(SectionKind::Keys, true, &[]),
            Err(ContainerWriteError::SectionCountMismatch { .. })
        ));
        assert!(ContainerWriter::new(Vec::new(), &metadata(), 2_000).is_err());

        let result =
            ContainerWriter::write_seekable(Cursor::new(Vec::new()), &metadata(), |writer| {
                for _ in 0..=MAX_SECTION_COUNT {
                    writer.write_section(SectionKind::Keys, true, &[])?;
                }
                Ok::<_, ContainerWriteError>(())
            });
        assert!(matches!(
            result,
            Err(ContainerWriteError::Format(FormatError::TooManySections { count, max }))
                if count == MAX_SECTION_COUNT + 1 && max == MAX_SECTION_COUNT
        ));
    }
}
