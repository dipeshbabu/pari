#![forbid(unsafe_code)]
//! Safe codecs and a versioned, non-executable index container for Pari.
//!
//! The format crate is deliberately independent from storage. Local files,
//! remote backends, Python bindings, and the CLI can share the same metadata
//! and framing rules without serializing language objects.

mod bucket_stream;
mod buckets;
mod codec;
mod deltas;
mod format;
mod layout;
mod signatures;
mod writer;

pub use bucket_stream::{
    encode_bucket_segment_parts, write_bucket_segment, BucketWriteRecord, StreamedBucketSegment,
};
pub use buckets::{
    bucket_record_size, decode_bucket_segment, encode_bucket_segment, read_bucket_members,
    validate_global_bucket_order, BucketDirectory, BucketError, BucketKey, BucketLocation,
    BucketRangeCursor, BucketRecord, BUCKET_SEGMENT_HEADER_BYTES, BUCKET_SEGMENT_TARGET_BYTES,
};
pub use codec::{
    BytesCodec, CodecError, CodecId, I64Codec, JsonValueCodec, KeyCodec, U64Codec, Utf8Codec,
};
pub use deltas::{DeltaCommit, DeltaLayout, NextDelta, COMMIT_TRAILER_BYTES, DELTA_HEADER_BYTES};
pub use format::{
    Algorithm, FormatError, IndexFile, IndexMetadata, Section, SectionKind, SignatureScheme,
    MAX_SECTION_BYTES,
};
pub use layout::{FileLayout, LayoutError, SectionDescriptor};
pub use signatures::{
    decode_signature_segment, encode_signature_segment, read_signature_row,
    read_signature_segment_values, signature_row_bytes, validate_signature_coverage,
    SignatureError, SignatureSegment, SIGNATURE_SEGMENT_HEADER_BYTES,
    SIGNATURE_SEGMENT_TARGET_BYTES,
};
pub use writer::{ContainerWriteError, ContainerWriter};
