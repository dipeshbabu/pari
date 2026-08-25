#![forbid(unsafe_code)]
//! Safe codecs and a versioned, non-executable index container for Pari.
//!
//! The format crate is deliberately independent from storage. Local files,
//! remote backends, Python bindings, and the CLI can share the same metadata
//! and framing rules without serializing language objects.

mod buckets;
mod codec;
mod format;
mod layout;

pub use buckets::{
    bucket_record_size, decode_bucket_segment, encode_bucket_segment, read_bucket_members,
    validate_global_bucket_order, BucketError, BucketKey, BucketLocation, BucketRecord,
    BUCKET_SEGMENT_HEADER_BYTES, BUCKET_SEGMENT_TARGET_BYTES,
};
pub use codec::{
    BytesCodec, CodecError, CodecId, I64Codec, JsonValueCodec, KeyCodec, U64Codec, Utf8Codec,
};
pub use format::{
    Algorithm, FormatError, IndexFile, IndexMetadata, Section, SectionKind, SignatureScheme,
};
pub use layout::{FileLayout, LayoutError, SectionDescriptor};
