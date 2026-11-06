//! Writing kine: the etcd API over SQL that gateways read their resources
//! from.
//!
//! kine takes exactly three transaction shapes (kine v0.17.5
//! `pkg/server/{create,update,delete}.go`, dispatched by `limited.go`), and
//! these are the only writes made here:
//!
//! - create: `mod_revision(key) == 0` → put;
//! - update: `mod_revision(key) == rev` → put, else get;
//! - delete: `mod_revision(key) == rev` → delete, else get.
//!
//! Never `DeleteRange` (kine answers `Unimplemented`), a transaction of two
//! writes, or a single-key Range with a `limit` (also `Unimplemented`). Every
//! write is a compare-and-swap on the key's mod revision, so a write that
//! arrives late, after its caller gave up on it, changes nothing that moved
//! on since.

use std::fmt;
use std::future::Future;
use std::time::Duration;

use async_trait::async_trait;
use etcd_client::{
    Client, Compare, CompareOp, ConnectOptions, GetOptions, KvClient, Txn, TxnOp, TxnOpResponse,
    TxnResponse,
};

/// How long one call to kine may take, from sending the request to reading
/// the last byte of the answer. A write that times out may still be
/// applied: the caller learns which from the next compare-and-swap.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// The page size of [`KineWriter::list`], the gateway's own
/// (`aisix-etcd` `LOAD_PAGE_SIZE`).
const PAGE_SIZE: i64 = 500;

/// The largest answer decoded from kine. kine merges whatever is pending
/// into one message, so tonic's 4 MiB default is no bound at all on what a
/// read returns; the gateway raises it the same way
/// (`aisix-etcd` `client::MAX_DECODING_MESSAGE_SIZE`).
const MAX_DECODING_MESSAGE_SIZE: usize = i32::MAX as usize;

/// A key's value and the revision it was last written at.
#[derive(Clone, PartialEq, Eq)]
pub struct KeyValue {
    pub value: Vec<u8>,
    pub mod_revision: i64,
}

/// The value's length only: a projected value holds plaintext credentials.
impl fmt::Debug for KeyValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeyValue")
            .field("value", &format_args!("<{} bytes>", self.value.len()))
            .field("mod_revision", &self.mod_revision)
            .finish()
    }
}

/// What a compare-and-swap write did.
#[derive(Clone, PartialEq, Eq)]
pub enum WriteOutcome {
    /// Written at `revision`, which for a put is the key's new mod revision.
    Applied { revision: i64 },
    /// The key was not at the expected revision: `current` is what it holds
    /// now (`None` when it does not exist).
    Conflict { current: Option<KeyValue> },
}

/// As [`KeyValue`]'s: never the value's bytes.
impl fmt::Debug for WriteOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WriteOutcome::Applied { revision } => f
                .debug_struct("Applied")
                .field("revision", revision)
                .finish(),
            WriteOutcome::Conflict { current } => f
                .debug_struct("Conflict")
                .field("current", current)
                .finish(),
        }
    }
}

/// A call to kine that failed. Never carries a value: kine's own messages
/// name the problem, not the data.
#[derive(Debug, thiserror::Error)]
pub enum KineError {
    /// kine could not be reached, or answered with an error.
    #[error("kine: {0}")]
    Unavailable(String),
    #[error("kine did not answer within {0:?}")]
    Timeout(Duration),
    /// An answer of a shape the request cannot produce.
    #[error("kine gave an unexpected answer to a {0}")]
    Unexpected(&'static str),
}

impl From<etcd_client::Error> for KineError {
    fn from(e: etcd_client::Error) -> KineError {
        KineError::Unavailable(e.to_string())
    }
}

/// The writes and reads the projector makes. Every write is a
/// compare-and-swap; see the module docs for the shapes.
#[async_trait]
pub trait KineWriter: Send + Sync {
    /// Writes `key` if it does not exist.
    async fn create(&self, key: &str, value: &[u8]) -> Result<WriteOutcome, KineError>;
    /// Overwrites `key` if it is at `expected_mod_revision`.
    async fn update(
        &self,
        key: &str,
        expected_mod_revision: i64,
        value: &[u8],
    ) -> Result<WriteOutcome, KineError>;
    /// Deletes `key` if it is at `expected_mod_revision`.
    async fn delete(
        &self,
        key: &str,
        expected_mod_revision: i64,
    ) -> Result<WriteOutcome, KineError>;
    async fn get(&self, key: &str) -> Result<Option<KeyValue>, KineError>;
    /// Every key under `prefix`, read in pages the way the gateway reads
    /// them, all pinned to the first page's revision, so the list is one
    /// moment's state. Keys are the bytes kine holds: one that is not
    /// UTF-8 is still listed, and cannot be mistaken for another.
    async fn list(&self, prefix: &str) -> Result<Vec<(Vec<u8>, KeyValue)>, KineError>;
}

/// kine, over its etcd API.
pub struct KineClient {
    client: Client,
}

impl KineClient {
    /// A client of the kine at `endpoint`: plaintext, since kine listens on
    /// loopback only. It connects on first use, so a kine that is down when
    /// the control plane boots is not an error here; only a malformed
    /// endpoint is.
    pub async fn connect(endpoint: &str) -> Result<KineClient, KineError> {
        let options = ConnectOptions::new().with_connect_timeout(CONNECT_TIMEOUT);
        let client = Client::connect([endpoint], Some(options)).await?;
        Ok(KineClient { client })
    }

    fn kv(&self) -> KvClient {
        self.client
            .kv_client()
            .max_decoding_message_size(MAX_DECODING_MESSAGE_SIZE)
    }

    /// Runs `call`, bounded by [`REQUEST_TIMEOUT`] as a whole, answer body
    /// included.
    async fn bounded<T>(
        call: impl Future<Output = Result<T, etcd_client::Error>>,
    ) -> Result<T, KineError> {
        tokio::time::timeout(REQUEST_TIMEOUT, call)
            .await
            .map_err(|_| KineError::Timeout(REQUEST_TIMEOUT))?
            .map_err(KineError::from)
    }

    /// A compare-and-swap whose `or_else` branch reads the key.
    async fn swap(
        &self,
        op: &'static str,
        key: &str,
        expected_mod_revision: i64,
        then: TxnOp,
    ) -> Result<WriteOutcome, KineError> {
        let txn = Txn::new()
            .when(vec![Compare::mod_revision(
                key,
                CompareOp::Equal,
                expected_mod_revision,
            )])
            .and_then(vec![then])
            .or_else(vec![TxnOp::get(key, None)]);
        let resp = Self::bounded(self.kv().txn(txn)).await?;
        if resp.succeeded() {
            return applied(op, &resp);
        }
        match resp.op_responses().into_iter().next() {
            Some(TxnOpResponse::Get(get)) => Ok(WriteOutcome::Conflict {
                current: get.kvs().first().map(key_value),
            }),
            _ => Err(KineError::Unexpected(op)),
        }
    }
}

fn key_value(kv: &etcd_client::KeyValue) -> KeyValue {
    KeyValue {
        value: kv.value().to_vec(),
        mod_revision: kv.mod_revision(),
    }
}

/// The revision a successful write was applied at: kine answers it in the
/// transaction's header.
fn applied(op: &'static str, resp: &TxnResponse) -> Result<WriteOutcome, KineError> {
    match resp.header().map(|h| h.revision()) {
        Some(revision) if revision > 0 => Ok(WriteOutcome::Applied { revision }),
        _ => Err(KineError::Unexpected(op)),
    }
}

/// The end of the range of keys that start with `prefix`: the prefix with
/// its last byte below 0xff incremented and the rest dropped (etcd's
/// `GetPrefixRangeEnd`).
fn prefix_range_end(prefix: &[u8]) -> Vec<u8> {
    let mut end = prefix.to_vec();
    while let Some(last) = end.pop() {
        if last < 0xff {
            end.push(last + 1);
            return end;
        }
    }
    vec![0]
}

#[async_trait]
impl KineWriter for KineClient {
    async fn create(&self, key: &str, value: &[u8]) -> Result<WriteOutcome, KineError> {
        // kine's create takes no `or_else`, and answers a key that exists
        // with nothing but `succeeded: false`.
        let txn = Txn::new()
            .when(vec![Compare::mod_revision(key, CompareOp::Equal, 0)])
            .and_then(vec![TxnOp::put(key, value, None)]);
        let resp = Self::bounded(self.kv().txn(txn)).await?;
        if resp.succeeded() {
            return applied("create", &resp);
        }
        Ok(WriteOutcome::Conflict {
            current: self.get(key).await?,
        })
    }

    async fn update(
        &self,
        key: &str,
        expected_mod_revision: i64,
        value: &[u8],
    ) -> Result<WriteOutcome, KineError> {
        let put = TxnOp::put(key, value, None);
        self.swap("update", key, expected_mod_revision, put).await
    }

    async fn delete(
        &self,
        key: &str,
        expected_mod_revision: i64,
    ) -> Result<WriteOutcome, KineError> {
        let delete = TxnOp::delete(key, None);
        self.swap("delete", key, expected_mod_revision, delete)
            .await
    }

    async fn get(&self, key: &str) -> Result<Option<KeyValue>, KineError> {
        let resp = Self::bounded(self.kv().get(key, None)).await?;
        Ok(resp.kvs().first().map(key_value))
    }

    async fn list(&self, prefix: &str) -> Result<Vec<(Vec<u8>, KeyValue)>, KineError> {
        let range_end = prefix_range_end(prefix.as_bytes());
        let mut start = prefix.as_bytes().to_vec();
        let mut revision = 0;
        let mut out = Vec::new();
        loop {
            let mut options = GetOptions::new()
                .with_range(range_end.clone())
                .with_limit(PAGE_SIZE);
            if revision > 0 {
                options = options.with_revision(revision);
            }
            let resp = Self::bounded(self.kv().get(start.clone(), Some(options))).await?;
            if revision == 0 {
                revision = resp.header().map_or(0, |h| h.revision());
            }
            out.extend(
                resp.kvs()
                    .iter()
                    .map(|kv| (kv.key().to_vec(), key_value(kv))),
            );
            match (resp.more(), resp.kvs().last()) {
                (true, Some(last)) => {
                    start = last.key().to_vec();
                    start.push(0);
                }
                _ => return Ok(out),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{prefix_range_end, KeyValue, WriteOutcome};

    #[test]
    fn a_value_is_never_shown_only_its_length() {
        let kv = KeyValue {
            value: b"sk-shown-7c1".to_vec(),
            mod_revision: 4,
        };
        let conflict = WriteOutcome::Conflict {
            current: Some(kv.clone()),
        };
        for shown in [format!("{kv:?}"), format!("{conflict:?}")] {
            assert!(!shown.contains("sk-shown-7c1"), "{shown}");
            // Nor the bytes as numbers (`115, 107, ...`).
            assert!(!shown.contains("115"), "{shown}");
            assert!(shown.contains("<12 bytes>"), "{shown}");
        }
    }

    #[test]
    fn a_prefix_range_ends_just_past_its_last_key() {
        assert_eq!(prefix_range_end(b"/aisix/e/"), b"/aisix/e0".to_vec());
        assert_eq!(prefix_range_end(b"a\xff"), b"b".to_vec());
        assert_eq!(prefix_range_end(b"\xff\xff"), vec![0]);
    }
}
