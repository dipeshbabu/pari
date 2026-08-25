#![forbid(unsafe_code)]
//! Safe codecs and a versioned, non-executable index container for Pari.
//!
//! The format crate is deliberately independent from storage. Local files,
//! remote backends, Python bindings, and the CLI can share the same metadata
//! and framing rules without serializing language objects.

mod codec;
mod format;
mod layout;

pub use codec::{
    BytesCodec, CodecError, CodecId, I64Codec, JsonValueCodec, KeyCodec, U64Codec, Utf8Codec,
};
pub use format::{
    Algorithm, FormatError, IndexFile, IndexMetadata, Section, SectionKind, SignatureScheme,
};
pub use layout::{FileLayout, LayoutError, SectionDescriptor};
