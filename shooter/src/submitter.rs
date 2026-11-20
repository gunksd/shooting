//! Submission channels. Each channel declares what it can do; the engine refuses a mode whose
//! required capability is missing instead of degrading silently.
//!
//! No channel adapter ever attaches a tip transfer: the only bid is the transaction's own gas price,
//! so sending the same signed transaction to several channels cannot double-pay or double-execute
//! (same hash, same nonce).
use crate::config::{ChannelConfig, ChannelKind};
use crate::rpc::{ChainRpc, HttpRpc, RpcError};
use alloy::primitives::Bytes;
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caps {
    /// Transaction becomes visible in the public mempool.
    pub public: bool,
    /// Single private transaction to a builder.
    pub private_tx: bool,
    /// Ordered atomic bundle of our own transactions.
    pub bundle: bool,
    /// Bundle whose first transactions are someone else's signed pending transactions (backrun).
    pub bundle_foreign_target: bool,
    /// Builder-side `maxBlockNumber`. A submission constraint, not protocol-level expiry.
    pub max_block_number: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ack {
    pub channel: String,
    pub elapsed_ms: u64,
    /// Ok means "the endpoint accepted the request". It never means included.
    pub result: Result<String, RpcError>,
}

#[async_trait]
pub trait Channel: Send + Sync {
    fn name(&self) -> &str;
    fn caps(&self) -> Caps;
    /// Lowest gas price the channel documents as acceptable.
    fn min_gas_price_wei(&self) -> u128;
    async fn send_tx(&self, raw: &Bytes) -> Result<String, RpcError>;
    async fn send_bundle(&self, txs: &[Bytes], max_block: u64) -> Result<String, RpcError>;
}

pub struct JsonRpcChannel {
    name: String,
    kind: ChannelKind,
    rpc: Arc<dyn ChainRpc>,
    min_interval: Duration,
    last: Mutex<Option<Instant>>,
}

impl JsonRpcChannel {
    pub fn from_config(c: &ChannelConfig, timeout: Duration) -> anyhow::Result<Self> {
        let auth = match &c.auth_env {
            Some(var) => Some(std::env::var(var).map_err(|_| {
                anyhow::anyhow!("channel {}: environment variable {var} is not set", c.name)
            })?),
            None => None,
        };
        if c.kind == ChannelKind::Blockrazor && auth.is_none() {
            anyhow::bail!(
                "channel {}: BlockRazor requires an auth token (auth_env)",
                c.name
            );
        }
        let rpc = HttpRpc::new(&c.url, auth, timeout).map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(Self::new(
            &c.name,
            c.kind,
            Arc::new(rpc),
            Duration::from_millis(c.min_interval_ms),
        ))
    }

    pub fn new(
        name: &str,
        kind: ChannelKind,
        rpc: Arc<dyn ChainRpc>,
        min_interval: Duration,
    ) -> Self {
        Self {
            name: name.into(),
            kind,
            rpc,
            min_interval,
            last: Mutex::new(None),
        }
    }

    async fn throttle(&self) {
        let mut last = self.last.lock().await;
        if let Some(t) = *last {
            let since = t.elapsed();
            if since < self.min_interval {
                tokio::time::sleep(self.min_interval - since).await;
            }
        }
        *last = Some(Instant::now());
    }
}

/// Capabilities per channel kind, from the vendors' public documentation (see docs/feasibility.md).
/// Anything the documentation does not state explicitly is `false` until proven on mainnet.
pub fn caps_of(kind: ChannelKind) -> Caps {
    match kind {
        ChannelKind::Public => Caps {
            public: true,
            private_tx: false,
            bundle: false,
            bundle_foreign_target: false,
            max_block_number: false,
        },
        // 48 Club documents eth_sendBundle / eth_sendPrivateTransaction; including a foreign public
        // transaction is only implied by its pricing rules, so it stays off.
        ChannelKind::Club48 => Caps {
            public: false,
            private_tx: true,
            bundle: true,
            bundle_foreign_target: false,
            max_block_number: true,
        },
        // BlockRazor's pricing example lists a mempool transaction inside a bundle explicitly.
        ChannelKind::Blockrazor => Caps {
            public: false,
            private_tx: true,
            bundle: true,
            bundle_foreign_target: true,
            max_block_number: true,
        },
    }
}

#[async_trait]
impl Channel for JsonRpcChannel {
    fn name(&self) -> &str {
        &self.name
    }
    fn caps(&self) -> Caps {
        caps_of(self.kind)
    }
    fn min_gas_price_wei(&self) -> u128 {
        50_000_000 // 0.05 gwei: BSC network minimum and both builders' documented bundle floor
    }

    async fn send_tx(&self, raw: &Bytes) -> Result<String, RpcError> {
        self.throttle().await;
        let method = match self.kind {
            ChannelKind::Public => "eth_sendRawTransaction",
            ChannelKind::Club48 | ChannelKind::Blockrazor => "eth_sendPrivateTransaction",
        };
        Ok(self.rpc.request(method, json!([raw])).await?.to_string())
    }

    async fn send_bundle(&self, txs: &[Bytes], max_block: u64) -> Result<String, RpcError> {
        if !self.caps().bundle {
            return Err(RpcError::Transport(format!(
                "channel {} has no bundle support",
                self.name
            )));
        }
        self.throttle().await;
        // No revertingTxHashes: if any transaction would revert the builder must drop the bundle.
        Ok(self
            .rpc
            .request(
                "eth_sendBundle",
                json!([{"txs": txs, "maxBlockNumber": max_block}]),
            )
            .await?
            .to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SubmitError {
    #[error("no selected channel supports {0}")]
    MissingCapability(&'static str),
    #[error("gas price {have} below channel {channel} minimum {min}")]
    BelowChannelMinimum {
        channel: String,
        have: u128,
        min: u128,
    },
}

/// Sends one signed transaction to every channel concurrently. Returns one ack per channel; a
/// timeout is reported as an error ack and the caller must still assume the transaction may land.
pub async fn broadcast_tx(
    channels: &[Arc<dyn Channel>],
    raw: &Bytes,
    gas_price: u128,
    timeout: Duration,
) -> Result<Vec<Ack>, SubmitError> {
    for c in channels {
        if gas_price < c.min_gas_price_wei() {
            return Err(SubmitError::BelowChannelMinimum {
                channel: c.name().into(),
                have: gas_price,
                min: c.min_gas_price_wei(),
            });
        }
    }
    let futs = channels.iter().map(|c| async move {
        let t = Instant::now();
        let result = match tokio::time::timeout(timeout, c.send_tx(raw)).await {
            Ok(r) => r,
            Err(_) => Err(RpcError::Timeout),
        };
        Ack {
            channel: c.name().to_string(),
            elapsed_ms: t.elapsed().as_millis() as u64,
            result,
        }
    });
    Ok(futures_util::future::join_all(futs).await)
}

/// Sends `[targets…, ours]` as a bundle to the channels that can carry foreign targets.
/// Channels without that capability are skipped; if none qualifies the call is refused.
pub async fn broadcast_backrun(
    channels: &[Arc<dyn Channel>],
    targets: &[Bytes],
    ours: &Bytes,
    gas_price: u128,
    max_block: u64,
    timeout: Duration,
) -> Result<Vec<Ack>, SubmitError> {
    let able: Vec<_> = channels
        .iter()
        .filter(|c| c.caps().bundle_foreign_target && c.caps().max_block_number)
        .collect();
    if able.is_empty() {
        return Err(SubmitError::MissingCapability(
            "bundle with foreign target and maxBlockNumber",
        ));
    }
    for c in &able {
        if gas_price < c.min_gas_price_wei() {
            return Err(SubmitError::BelowChannelMinimum {
                channel: c.name().into(),
                have: gas_price,
                min: c.min_gas_price_wei(),
            });
        }
    }
    let mut txs = targets.to_vec();
    txs.push(ours.clone());
    let txs = &txs;
    let futs = able.into_iter().map(|c| async move {
        let t = Instant::now();
        let result = match tokio::time::timeout(timeout, c.send_bundle(txs, max_block)).await {
            Ok(r) => r,
            Err(_) => Err(RpcError::Timeout),
        };
        Ack {
            channel: c.name().to_string(),
            elapsed_ms: t.elapsed().as_millis() as u64,
            result,
        }
    });
    Ok(futures_util::future::join_all(futs).await)
}
