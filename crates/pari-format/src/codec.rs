use std::{error::Error, fmt, str};

use serde_json::Value;

const MAX_KEY_BYTES: usize = 16 * 1024 * 1024;

/// Stable identifier for a key codec in persisted Pari metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum CodecId {
    /// Arbitrary bytes.
    Bytes = 1,
    /// UTF-8 strings.
    Utf8 = 2,
    /// Little-endian unsigned 64-bit integers.
    U64 = 3,
    /// Little-endian signed 64-bit integers.
    I64 = 4,
    /// JSON values encoded as UTF-8 JSON text.
    Json = 5,
}

impl CodecId {
    pub(crate) const fn from_raw(value: u16) -> Option<Self> {
        match value {
            1 => Some(Self::Bytes),
            2 => Some(Self::Utf8),
            3 => Some(Self::U64),
            4 => Some(Self::I64),
            5 => Some(Self::Json),
            _ => None,
        }
    }

    pub(crate) const fn as_raw(self) -> u16 {
        self as u16
    }
}

/// Errors produced by safe key codecs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodecError {
    /// The key exceeds the size accepted by the in-memory codec API.
    PayloadTooLarge { actual: usize, max: usize },
    /// A fixed-width codec received the wrong number of bytes.
    InvalidLength { expected: usize, actual: usize },
    /// String bytes are not valid UTF-8.
    InvalidUtf8,
    /// JSON encoding or decoding failed.
    InvalidJson { message: String },
}

impl fmt::Display for CodecError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PayloadTooLarge { actual, max } => {
                write!(formatter, "key payload is {actual} bytes; maximum is {max}")
            }
            Self::InvalidLength { expected, actual } => write!(
                formatter,
                "invalid key length: expected {expected} bytes, got {actual}"
            ),
            Self::InvalidUtf8 => formatter.write_str("key bytes are not valid UTF-8"),
            Self::InvalidJson { message } => write!(formatter, "invalid JSON key: {message}"),
        }
    }
}

impl Error for CodecError {}

/// Encode and decode one supported key type without executable deserialization.
pub trait KeyCodec<T> {
    /// Return the stable persisted codec identifier.
    fn id(&self) -> CodecId;

    /// Encode a key to a bounded byte representation.
    fn encode(&self, value: &T) -> Result<Vec<u8>, CodecError>;

    /// Decode a key from its byte representation.
    fn decode(&self, bytes: &[u8]) -> Result<T, CodecError>;
}

/// Codec for arbitrary byte-vector keys.
#[derive(Debug, Default, Clone, Copy)]
pub struct BytesCodec;

impl KeyCodec<Vec<u8>> for BytesCodec {
    fn id(&self) -> CodecId {
        CodecId::Bytes
    }

    fn encode(&self, value: &Vec<u8>) -> Result<Vec<u8>, CodecError> {
        check_payload_size(value.len())?;
        Ok(value.clone())
    }

    fn decode(&self, bytes: &[u8]) -> Result<Vec<u8>, CodecError> {
        check_payload_size(bytes.len())?;
        Ok(bytes.to_vec())
    }
}

/// Codec for owned UTF-8 string keys.
#[derive(Debug, Default, Clone, Copy)]
pub struct Utf8Codec;

impl KeyCodec<String> for Utf8Codec {
    fn id(&self) -> CodecId {
        CodecId::Utf8
    }

    fn encode(&self, value: &String) -> Result<Vec<u8>, CodecError> {
        check_payload_size(value.len())?;
        Ok(value.as_bytes().to_vec())
    }

    fn decode(&self, bytes: &[u8]) -> Result<String, CodecError> {
        check_payload_size(bytes.len())?;
        str::from_utf8(bytes)
            .map(str::to_owned)
            .map_err(|_| CodecError::InvalidUtf8)
    }
}

/// Codec for unsigned 64-bit integer keys.
#[derive(Debug, Default, Clone, Copy)]
pub struct U64Codec;

impl KeyCodec<u64> for U64Codec {
    fn id(&self) -> CodecId {
        CodecId::U64
    }

    fn encode(&self, value: &u64) -> Result<Vec<u8>, CodecError> {
        Ok(value.to_le_bytes().to_vec())
    }

    fn decode(&self, bytes: &[u8]) -> Result<u64, CodecError> {
        let raw = exact_array::<8>(bytes)?;
        Ok(u64::from_le_bytes(raw))
    }
}

/// Codec for signed 64-bit integer keys.
#[derive(Debug, Default, Clone, Copy)]
pub struct I64Codec;

impl KeyCodec<i64> for I64Codec {
    fn id(&self) -> CodecId {
        CodecId::I64
    }

    fn encode(&self, value: &i64) -> Result<Vec<u8>, CodecError> {
        Ok(value.to_le_bytes().to_vec())
    }

    fn decode(&self, bytes: &[u8]) -> Result<i64, CodecError> {
        let raw = exact_array::<8>(bytes)?;
        Ok(i64::from_le_bytes(raw))
    }
}

/// Codec for [`serde_json::Value`] keys.
///
/// JSON is intentionally data-only. Decoding cannot construct executable Rust
/// or Python objects, which keeps persisted key handling separate from language
/// object serialization.
#[derive(Debug, Default, Clone, Copy)]
pub struct JsonValueCodec;

impl KeyCodec<Value> for JsonValueCodec {
    fn id(&self) -> CodecId {
        CodecId::Json
    }

    fn encode(&self, value: &Value) -> Result<Vec<u8>, CodecError> {
        let bytes = serde_json::to_vec(value).map_err(|error| CodecError::InvalidJson {
            message: error.to_string(),
        })?;
        check_payload_size(bytes.len())?;
        Ok(bytes)
    }

    fn decode(&self, bytes: &[u8]) -> Result<Value, CodecError> {
        check_payload_size(bytes.len())?;
        serde_json::from_slice(bytes).map_err(|error| CodecError::InvalidJson {
            message: error.to_string(),
        })
    }
}

fn check_payload_size(length: usize) -> Result<(), CodecError> {
    if length > MAX_KEY_BYTES {
        return Err(CodecError::PayloadTooLarge {
            actual: length,
            max: MAX_KEY_BYTES,
        });
    }
    Ok(())
}

fn exact_array<const N: usize>(bytes: &[u8]) -> Result<[u8; N], CodecError> {
    bytes.try_into().map_err(|_| CodecError::InvalidLength {
        expected: N,
        actual: bytes.len(),
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        BytesCodec, CodecError, CodecId, I64Codec, JsonValueCodec, KeyCodec, U64Codec, Utf8Codec,
    };

    #[test]
    fn bytes_round_trip() {
        let codec = BytesCodec;
        let value = vec![0, 1, 2, 255];
        assert_eq!(codec.id(), CodecId::Bytes);
        assert_eq!(codec.decode(&codec.encode(&value).expect("encode")).expect("decode"), value);
    }

    #[test]
    fn utf8_round_trip_and_invalid_utf8() {
        let codec = Utf8Codec;
        let value = String::from("Pari similarity");
        assert_eq!(codec.decode(&codec.encode(&value).expect("encode")).expect("decode"), value);
        assert_eq!(codec.decode(&[0xFF]), Err(CodecError::InvalidUtf8));
    }

    #[test]
    fn integer_codecs_are_little_endian_and_exact_width() {
        let unsigned = U64Codec;
        assert_eq!(unsigned.encode(&0x0102_0304_0506_0708).expect("encode"), vec![8, 7, 6, 5, 4, 3, 2, 1]);
        assert_eq!(unsigned.decode(&[8, 7, 6, 5, 4, 3, 2, 1]).expect("decode"), 0x0102_0304_0506_0708);
        assert!(matches!(unsigned.decode(&[1, 2]), Err(CodecError::InvalidLength { expected: 8, actual: 2 })));

        let signed = I64Codec;
        assert_eq!(signed.decode(&signed.encode(&-42).expect("encode")).expect("decode"), -42);
    }

    #[test]
    fn json_round_trip_is_data_only() {
        let codec = JsonValueCodec;
        let value = json!({"id": 7, "tags": ["near", "duplicate"]});
        let encoded = codec.encode(&value).expect("encode");
        assert_eq!(codec.decode(&encoded).expect("decode"), value);
        assert!(matches!(codec.decode(b"{"), Err(CodecError::InvalidJson { .. })));
    }
}
