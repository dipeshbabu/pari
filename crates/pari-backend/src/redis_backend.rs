use std::fmt;

use pari_format::BucketKey;

use crate::{
    decode_descriptor, decode_user_key, encode_descriptor, encode_user_key, BackendCapabilities,
    BackendError, BackendStats, IndexDescriptor, StorageBackend, StoredItem,
};

const MEMBER_BYTES: usize = 20;
const BUCKET_PREFIX_BYTES: usize = 12;
const KEY_BYTES: usize = 8;
const MAX_NAMESPACE_BYTES: usize = 128;

const INITIALIZE_SCRIPT: &str = include_str!("scripts/initialize.lua");
const INSERT_SCRIPT: &str = include_str!("scripts/insert.lua");
const DELETE_SCRIPT: &str = include_str!("scripts/delete.lua");

#[derive(Debug)]
struct RedisKeys {
    meta: String,
    records: String,
    buckets: String,
}

impl RedisKeys {
    fn new(namespace: &str) -> Self {
        let prefix = format!("pari:{namespace}");
        Self {
            meta: format!("{prefix}:meta"),
            records: format!("{prefix}:records"),
            buckets: format!("{prefix}:buckets"),
        }
    }
}

/// Redis-backed implementation of Pari's typed [`StorageBackend`] contract.
///
/// Each namespace owns exactly three Redis keys: immutable index metadata, a
/// hash of live external keys, and one lexicographically indexed sorted set of
/// bucket memberships. Fixed namespace ownership makes cleanup and TTL behavior
/// deterministic and avoids creating one Redis key per LSH bucket.
pub struct RedisBackend {
    connection: redis::Connection,
    namespace: String,
    keys: RedisKeys,
    retention_seconds: Option<u64>,
    round_trips: u64,
}

impl fmt::Debug for RedisBackend {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RedisBackend")
            .field("namespace", &self.namespace)
            .field("retention_seconds", &self.retention_seconds)
            .field("round_trips", &self.round_trips)
            .finish_non_exhaustive()
    }
}

impl RedisBackend {
    /// Connect to Redis and bind this handle to one isolated Pari namespace.
    ///
    /// The URL is used only to establish the connection and is never retained
    /// in errors or debug output, so credentials cannot be leaked accidentally.
    pub fn connect(url: &str, namespace: &str) -> Result<Self, BackendError> {
        validate_namespace(namespace)?;
        let client =
            redis::Client::open(url).map_err(|error| redis_error("client setup", &error))?;
        let connection = client
            .get_connection()
            .map_err(|error| redis_error("connect", &error))?;
        Ok(Self {
            connection,
            namespace: namespace.to_owned(),
            keys: RedisKeys::new(namespace),
            retention_seconds: None,
            round_trips: 0,
        })
    }

    /// Return the isolated namespace owned by this backend handle.
    #[must_use]
    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    fn retention_argument(&self) -> u64 {
        self.retention_seconds.unwrap_or(0)
    }

    fn record_blob(item: &StoredItem) -> Result<Vec<u8>, BackendError> {
        let capacity = item
            .band_hashes()
            .len()
            .checked_mul(MEMBER_BYTES)
            .ok_or(BackendError::LengthOverflow)?;
        let mut blob = Vec::with_capacity(capacity);
        for (band, hash) in item.band_hashes().iter().copied().enumerate() {
            let band = u32::try_from(band).map_err(|_| BackendError::LengthOverflow)?;
            blob.extend_from_slice(&encode_bucket_member(
                BucketKey::new(band, hash),
                item.key(),
            )?);
        }
        Ok(blob)
    }

    fn bump_round_trip(&mut self) {
        self.round_trips = self.round_trips.saturating_add(1);
    }
}

impl StorageBackend for RedisBackend {
    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities::REDIS
    }

    fn initialize(&mut self, descriptor: &IndexDescriptor) -> Result<(), BackendError> {
        let payload = encode_descriptor(descriptor)?;
        let ttl = descriptor.retention().map_or(0, |value| value.as_secs());
        self.bump_round_trip();
        let result: i64 = redis::cmd("EVAL")
            .arg(INITIALIZE_SCRIPT)
            .arg(3)
            .arg(&self.keys.meta)
            .arg(&self.keys.records)
            .arg(&self.keys.buckets)
            .arg(payload)
            .arg(ttl)
            .query(&mut self.connection)
            .map_err(|error| redis_error("initialize", &error))?;
        if result != 1 {
            return Err(BackendError::AlreadyExists);
        }
        self.retention_seconds = descriptor.retention().map(|value| value.as_secs());
        Ok(())
    }

    fn load_descriptor(&mut self) -> Result<IndexDescriptor, BackendError> {
        self.bump_round_trip();
        let payload: Option<Vec<u8>> = redis::cmd("GET")
            .arg(&self.keys.meta)
            .query(&mut self.connection)
            .map_err(|error| redis_error("load descriptor", &error))?;
        let payload = payload.ok_or(BackendError::NotFound)?;
        let descriptor = decode_descriptor(&payload)?;
        self.retention_seconds = descriptor.retention().map(|value| value.as_secs());
        Ok(descriptor)
    }

    fn contains_many(&mut self, keys: &[u64]) -> Result<Vec<bool>, BackendError> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let fields = keys
            .iter()
            .copied()
            .map(encode_user_key)
            .collect::<Result<Vec<_>, _>>()?;
        let mut pipeline = redis::pipe();
        for field in fields {
            pipeline.cmd("HEXISTS").arg(&self.keys.records).arg(field);
        }
        self.bump_round_trip();
        pipeline
            .query(&mut self.connection)
            .map_err(|error| redis_error("batch contains", &error))
    }

    fn insert_many(&mut self, items: &[StoredItem]) -> Result<(), BackendError> {
        if items.is_empty() {
            return Ok(());
        }
        let mut encoded = Vec::with_capacity(items.len());
        for item in items {
            encoded.push((encode_user_key(item.key())?, Self::record_blob(item)?));
        }

        let mut command = redis::cmd("EVAL");
        command
            .arg(INSERT_SCRIPT)
            .arg(3)
            .arg(&self.keys.meta)
            .arg(&self.keys.records)
            .arg(&self.keys.buckets)
            .arg(self.retention_argument())
            .arg(items.len());
        for (field, blob) in encoded {
            command.arg(field).arg(blob);
        }

        self.bump_round_trip();
        let (code, detail): (i64, i64) = command
            .query(&mut self.connection)
            .map_err(|error| redis_error("batch insert", &error))?;
        match code {
            1 => Ok(()),
            0 => Err(BackendError::NotFound),
            2 => {
                let index = usize::try_from(detail.saturating_sub(1))
                    .map_err(|_| BackendError::LengthOverflow)?;
                let item = items.get(index).ok_or_else(|| BackendError::CorruptData {
                    reason: "Redis duplicate response referenced an invalid batch item".to_owned(),
                })?;
                Err(BackendError::DuplicateKey { key: item.key() })
            }
            3 => Err(BackendError::CorruptData {
                reason: "Redis rejected a malformed bucket membership record".to_owned(),
            }),
            _ => Err(BackendError::CorruptData {
                reason: format!("Redis insert script returned unknown status {code}"),
            }),
        }
    }

    fn query_buckets(&mut self, buckets: &[BucketKey]) -> Result<Vec<Vec<u64>>, BackendError> {
        if buckets.is_empty() {
            return Ok(Vec::new());
        }
        let mut pipeline = redis::pipe();
        for bucket in buckets {
            let (lower, upper) = bucket_lex_range(*bucket);
            pipeline
                .cmd("ZRANGEBYLEX")
                .arg(&self.keys.buckets)
                .arg(lower)
                .arg(upper);
        }
        self.bump_round_trip();
        let raw: Vec<Vec<Vec<u8>>> = pipeline
            .query(&mut self.connection)
            .map_err(|error| redis_error("batch bucket query", &error))?;
        if raw.len() != buckets.len() {
            return Err(BackendError::CorruptData {
                reason: "Redis returned the wrong number of bucket result rows".to_owned(),
            });
        }
        raw.into_iter()
            .zip(buckets.iter().copied())
            .map(|(members, bucket)| {
                members
                    .into_iter()
                    .map(|member| decode_bucket_member(bucket, &member))
                    .collect()
            })
            .collect()
    }

    fn delete_many(&mut self, keys: &[u64]) -> Result<usize, BackendError> {
        if keys.is_empty() {
            return Ok(0);
        }
        let fields = keys
            .iter()
            .copied()
            .map(encode_user_key)
            .collect::<Result<Vec<_>, _>>()?;
        let mut command = redis::cmd("EVAL");
        command
            .arg(DELETE_SCRIPT)
            .arg(3)
            .arg(&self.keys.meta)
            .arg(&self.keys.records)
            .arg(&self.keys.buckets)
            .arg(self.retention_argument());
        for field in fields {
            command.arg(field);
        }
        self.bump_round_trip();
        let removed: i64 = command
            .query(&mut self.connection)
            .map_err(|error| redis_error("batch delete", &error))?;
        match removed {
            -1 => Err(BackendError::NotFound),
            -2 => Err(BackendError::CorruptData {
                reason: "Redis record payload has an invalid bucket-member width".to_owned(),
            }),
            value if value >= 0 => usize::try_from(value).map_err(|_| BackendError::LengthOverflow),
            value => Err(BackendError::CorruptData {
                reason: format!("Redis delete script returned unknown status {value}"),
            }),
        }
    }

    fn flush(&mut self) -> Result<(), BackendError> {
        self.bump_round_trip();
        let response: String = redis::cmd("PING")
            .query(&mut self.connection)
            .map_err(|error| redis_error("flush barrier", &error))?;
        if response == "PONG" {
            Ok(())
        } else {
            Err(BackendError::CorruptData {
                reason: "Redis flush barrier returned an unexpected response".to_owned(),
            })
        }
    }

    fn health(&mut self) -> Result<(), BackendError> {
        self.bump_round_trip();
        let response: String = redis::cmd("PING")
            .query(&mut self.connection)
            .map_err(|error| redis_error("health check", &error))?;
        if response == "PONG" {
            Ok(())
        } else {
            Err(BackendError::Transport {
                operation: "health check",
                message: "unexpected Redis PING response".to_owned(),
            })
        }
    }

    fn stats(&mut self) -> Result<BackendStats, BackendError> {
        let mut pipeline = redis::pipe();
        pipeline
            .cmd("HLEN")
            .arg(&self.keys.records)
            .cmd("ZCARD")
            .arg(&self.keys.buckets)
            .cmd("TTL")
            .arg(&self.keys.meta);
        self.bump_round_trip();
        let (items, memberships, ttl): (u64, u64, i64) = pipeline
            .query(&mut self.connection)
            .map_err(|error| redis_error("stats", &error))?;
        if ttl == -2 {
            return Err(BackendError::NotFound);
        }
        let ttl_seconds_remaining = if ttl < 0 {
            None
        } else {
            Some(u64::try_from(ttl).map_err(|_| BackendError::LengthOverflow)?)
        };
        Ok(BackendStats {
            items,
            bucket_memberships: memberships,
            round_trips: self.round_trips,
            ttl_seconds_remaining,
            bucket_distribution: None,
            queries: None,
        })
    }

    fn cleanup(&mut self) -> Result<(), BackendError> {
        self.bump_round_trip();
        let _: u64 = redis::cmd("DEL")
            .arg(&[
                self.keys.meta.as_str(),
                self.keys.records.as_str(),
                self.keys.buckets.as_str(),
            ])
            .query(&mut self.connection)
            .map_err(|error| redis_error("namespace cleanup", &error))?;
        self.retention_seconds = None;
        Ok(())
    }
}

fn validate_namespace(namespace: &str) -> Result<(), BackendError> {
    if namespace.is_empty()
        || namespace.len() > MAX_NAMESPACE_BYTES
        || !namespace
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(BackendError::InvalidNamespace {
            reason: "must be 1..=128 ASCII alphanumeric, '.', '_' or '-' bytes".to_owned(),
        });
    }
    Ok(())
}

fn encode_bucket_member(bucket: BucketKey, key: u64) -> Result<[u8; MEMBER_BYTES], BackendError> {
    let key = encode_user_key(key)?;
    let key: [u8; KEY_BYTES] = key.try_into().map_err(|_| BackendError::CorruptData {
        reason: "u64 key codec returned an unexpected width".to_owned(),
    })?;
    let mut member = [0_u8; MEMBER_BYTES];
    member[..4].copy_from_slice(&bucket.band().to_be_bytes());
    member[4..BUCKET_PREFIX_BYTES].copy_from_slice(&bucket.hash().to_be_bytes());
    member[BUCKET_PREFIX_BYTES..].copy_from_slice(&key);
    Ok(member)
}

fn decode_bucket_member(bucket: BucketKey, member: &[u8]) -> Result<u64, BackendError> {
    if member.len() != MEMBER_BYTES {
        return Err(BackendError::CorruptData {
            reason: format!(
                "Redis bucket member is {} bytes; expected {MEMBER_BYTES}",
                member.len()
            ),
        });
    }
    let expected = bucket_prefix(bucket);
    if member[..BUCKET_PREFIX_BYTES] != expected {
        return Err(BackendError::CorruptData {
            reason: "Redis bucket member prefix does not match the queried bucket".to_owned(),
        });
    }
    decode_user_key(&member[BUCKET_PREFIX_BYTES..])
}

fn bucket_prefix(bucket: BucketKey) -> [u8; BUCKET_PREFIX_BYTES] {
    let mut prefix = [0_u8; BUCKET_PREFIX_BYTES];
    prefix[..4].copy_from_slice(&bucket.band().to_be_bytes());
    prefix[4..].copy_from_slice(&bucket.hash().to_be_bytes());
    prefix
}

fn bucket_lex_range(bucket: BucketKey) -> (Vec<u8>, Vec<u8>) {
    let prefix = bucket_prefix(bucket);
    let mut lower = Vec::with_capacity(1 + MEMBER_BYTES);
    lower.push(b'[');
    lower.extend_from_slice(&prefix);
    lower.extend_from_slice(&[0_u8; KEY_BYTES]);
    let mut upper = Vec::with_capacity(1 + MEMBER_BYTES);
    upper.push(b'[');
    upper.extend_from_slice(&prefix);
    upper.extend_from_slice(&[u8::MAX; KEY_BYTES]);
    (lower, upper)
}

fn redis_error(operation: &'static str, error: &redis::RedisError) -> BackendError {
    BackendError::Transport {
        operation,
        message: format!("Redis {:?}", error.kind()),
    }
}
