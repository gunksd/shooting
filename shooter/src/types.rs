use alloy::primitives::{Address, Bytes, B256, U256};
use serde::Deserialize;

pub fn hex_u64(s: &str) -> Option<u64> {
    u64::from_str_radix(s.trim_start_matches("0x"), 16).ok()
}

fn de_u64<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    let s = String::deserialize(d)?;
    hex_u64(&s).ok_or_else(|| serde::de::Error::custom("bad hex quantity"))
}

fn de_opt_u64<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
    Ok(Option::<String>::deserialize(d)?.and_then(|s| hex_u64(&s)))
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Log {
    pub address: Address,
    pub topics: Vec<B256>,
    pub data: Bytes,
    #[serde(deserialize_with = "de_u64")]
    pub block_number: u64,
    pub block_hash: B256,
    pub transaction_hash: B256,
    #[serde(deserialize_with = "de_u64")]
    pub transaction_index: u64,
    #[serde(deserialize_with = "de_u64")]
    pub log_index: u64,
    #[serde(default)]
    pub removed: bool,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Head {
    #[serde(deserialize_with = "de_u64")]
    pub number: u64,
    pub hash: B256,
    pub parent_hash: B256,
    #[serde(deserialize_with = "de_u64")]
    pub timestamp: u64,
}

/// A pending transaction as seen on a public mempool feed. `raw` is filled in once the signed bytes
/// were fetched; only then can it be used as a bundle target.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PendingTx {
    pub hash: B256,
    pub from: Address,
    pub to: Option<Address>,
    pub input: Bytes,
    #[serde(skip)]
    pub raw: Option<Bytes>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Receipt {
    pub transaction_hash: B256,
    #[serde(deserialize_with = "de_u64")]
    pub block_number: u64,
    pub block_hash: B256,
    #[serde(deserialize_with = "de_u64")]
    pub transaction_index: u64,
    #[serde(deserialize_with = "de_u64")]
    pub gas_used: u64,
    #[serde(default, deserialize_with = "de_opt_u64")]
    pub status: Option<u64>,
    pub effective_gas_price: Option<U256>,
    #[serde(default)]
    pub logs: Vec<Log>,
}

/// Events delivered by the watcher. `source` is the index of the upstream that saw it first.
#[derive(Debug, Clone)]
pub enum ChainEvent {
    Head {
        head: Head,
        source: usize,
        seen_ms: u64,
    },
    Log {
        log: Log,
        source: usize,
        seen_ms: u64,
    },
    /// A previously delivered log was reverted by a reorg (`removed: true`).
    LogRemoved { log: Log, source: usize },
    Pending {
        tx: PendingTx,
        source: usize,
        seen_ms: u64,
    },
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
