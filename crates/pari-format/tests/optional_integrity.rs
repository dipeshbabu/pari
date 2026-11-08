use std::io::{self, Cursor, Read, Seek, SeekFrom};

use pari_format::{
    Algorithm, CodecId, FileLayout, IndexFile, IndexMetadata, Section, SectionKind, SignatureScheme,
};

struct BoundedReader(Cursor<Vec<u8>>);

impl Read for BoundedReader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        assert!(
            output.len() <= 64 * 1024,
            "optional verification allocated a large read"
        );
        self.0.read(output)
    }
}

impl Seek for BoundedReader {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.0.seek(position)
    }
}

#[test]
fn known_optional_sections_are_stream_checked_before_being_ignored() {
    for kind in [
        SectionKind::Keys,
        SectionKind::BandHashes,
        SectionKind::Buckets,
        SectionKind::Tombstones,
    ] {
        let metadata = IndexMetadata::new(
            Algorithm::MinHashLsh,
            SignatureScheme::PariAffine32V1,
            CodecId::U64,
            8,
            7,
            0.8,
            2,
            4,
            0,
        )
        .expect("metadata");
        let payload = vec![37; 2 * 64 * 1024 + 7];
        let section = Section::new(kind, false, payload).expect("optional section");
        let mut bytes = IndexFile::new(metadata, vec![section])
            .expect("container")
            .encode()
            .expect("encode");
        let valid = FileLayout::read_from(&mut BoundedReader(Cursor::new(bytes.clone())))
            .expect("valid optional payload");
        assert_eq!(valid.sections().len(), 1);
        *bytes.last_mut().expect("payload byte") ^= 1;
        assert!(IndexFile::decode(&bytes).is_err());
        assert!(
            FileLayout::read_from(&mut BoundedReader(Cursor::new(bytes))).is_err(),
            "accepted corrupt known optional {kind:?}"
        );
    }
}
