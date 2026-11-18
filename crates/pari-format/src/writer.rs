//! Streaming writer for the version-1 container.
//!
//! [`ContainerWriter`] produces exactly the bytes of [`crate::IndexFile::encode`]
//! while writing each section as soon as it is ready, so a writer never has to
//! hold the complete container in memory.

use std::{
    error::Error,
    fmt,
    io::{self, Write},
};

use crc32fast::hash as crc32;

use crate::{
    format::{encode_header, MAX_SECTION_BYTES, MAX_SECTION_COUNT, SECTION_FLAG_REQUIRED},
    FormatError, IndexMetadata, SectionKind,
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
/// The section count is part of the header, so it is declared up front and
/// [`ContainerWriter::finish`] verifies that exactly that many sections were
/// written.
#[derive(Debug)]
pub struct ContainerWriter<W: Write> {
    writer: W,
    declared: u32,
    written: u32,
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
            declared,
            written: 0,
        })
    }

    /// Frame and write one section payload.
    pub fn write_section(
        &mut self,
        kind: SectionKind,
        required: bool,
        payload: &[u8],
    ) -> Result<(), ContainerWriteError> {
        if self.written >= self.declared {
            return Err(ContainerWriteError::SectionCountMismatch {
                declared: self.declared,
                written: self.written.saturating_add(1),
            });
        }
        let length = u64::try_from(payload.len()).map_err(|_| FormatError::LengthOverflow)?;
        if length > MAX_SECTION_BYTES {
            return Err(FormatError::SectionTooLarge { length }.into());
        }
        let flags = if required { SECTION_FLAG_REQUIRED } else { 0 };
        self.writer.write_all(&(kind as u16).to_le_bytes())?;
        self.writer.write_all(&flags.to_le_bytes())?;
        self.writer.write_all(&length.to_le_bytes())?;
        self.writer.write_all(&crc32(payload).to_le_bytes())?;
        self.writer.write_all(payload)?;
        self.written += 1;
        Ok(())
    }

    /// Verify the declared section count and return the underlying writer.
    pub fn finish(self) -> Result<W, ContainerWriteError> {
        if self.written != self.declared {
            return Err(ContainerWriteError::SectionCountMismatch {
                declared: self.declared,
                written: self.written,
            });
        }
        Ok(self.writer)
    }
}

#[cfg(test)]
mod tests {
    use super::{ContainerWriteError, ContainerWriter};
    use crate::{
        Algorithm, CodecId, IndexFile, IndexMetadata, Section, SectionKind, SignatureScheme,
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
    }
}
