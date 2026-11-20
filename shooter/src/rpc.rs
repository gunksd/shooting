use crate::types::{hex_u64, Head, Log, Receipt};
use alloy::primitives::{Address, Bytes, B256, U256};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::time::Duration;

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum RpcError {
    #[error("transport: {0}")]
    Transport(String),
    #[error("timeout")]
    Timeout,
    #[error("rpc error {code}: {message}")]
    Rpc {
        code: i64,
        message: String,
        data: Option<String>,
    },
    #[error("decode: {0}")]
    Decode(String),
}

/// Minimal JSON-RPC surface. Every chain interaction goes through this trait so the engine can be
/// driven by a real node, by anvil, or by an in-memory chain in tests.
#[async_trait]
pub trait ChainRpc: Send + Sync {
    async fn request(&self, method: &str, params: Value) -> Result<Value, RpcError>;
}

pub struct HttpRpc {
    client: reqwest::Client,
    url: String,
    auth: Option<String>,
}

impl HttpRpc {
    /// `auth` is sent as the `Authorization` header and never logged.
    pub fn new(url: &str, auth: Option<String>, timeout: Duration) -> Result<Self, RpcError> {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .tcp_nodelay(true)
            .pool_idle_timeout(Duration::from_secs(90))
            .build()
            .map_err(|e| RpcError::Transport(e.to_string()))?;
        Ok(Self {
            client,
            url: url.to_string(),
            auth,
        })
    }
}

impl std::fmt::Debug for HttpRpc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HttpRpc({})", redact_url(&self.url))
    }
}

/// Keeps scheme and host, drops path/query because provider URLs often embed API keys.
pub fn redact_url(url: &str) -> String {
    match url.split_once("://") {
        Some((scheme, rest)) => format!(
            "{scheme}://{}/…",
            rest.split(['/', '?']).next().unwrap_or("")
        ),
        None => "…".into(),
    }
}

#[async_trait]
impl ChainRpc for HttpRpc {
    async fn request(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let mut req = self
            .client
            .post(&self.url)
            .json(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}));
        if let Some(a) = &self.auth {
            req = req.header("Authorization", a);
        }
        let resp = req.send().await.map_err(|e| {
            if e.is_timeout() {
                RpcError::Timeout
            } else {
                RpcError::Transport(e.without_url().to_string())
            }
        })?;
        let v: Value = resp
            .json()
            .await
            .map_err(|e| RpcError::Decode(e.without_url().to_string()))?;
        parse_response(v)
    }
}

pub fn parse_response(v: Value) -> Result<Value, RpcError> {
    if let Some(e) = v.get("error").filter(|e| !e.is_null()) {
        return Err(RpcError::Rpc {
            code: e.get("code").and_then(Value::as_i64).unwrap_or(0),
            message: e
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            data: e.get("data").map(|d| {
                d.as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| d.to_string())
            }),
        });
    }
    v.get("result")
        .cloned()
        .ok_or_else(|| RpcError::Decode("missing result".into()))
}

fn as_u64(v: &Value) -> Result<u64, RpcError> {
    v.as_str()
        .and_then(hex_u64)
        .ok_or_else(|| RpcError::Decode(format!("expected quantity, got {v}")))
}

fn de<T: serde::de::DeserializeOwned>(v: Value) -> Result<T, RpcError> {
    serde_json::from_value(v).map_err(|e| RpcError::Decode(e.to_string()))
}

pub async fn block_number(rpc: &dyn ChainRpc) -> Result<u64, RpcError> {
    as_u64(&rpc.request("eth_blockNumber", json!([])).await?)
}

pub async fn chain_id(rpc: &dyn ChainRpc) -> Result<u64, RpcError> {
    as_u64(&rpc.request("eth_chainId", json!([])).await?)
}

pub async fn gas_price(rpc: &dyn ChainRpc) -> Result<u128, RpcError> {
    as_u64(&rpc.request("eth_gasPrice", json!([])).await?).map(u128::from)
}

pub async fn nonce_at(rpc: &dyn ChainRpc, who: Address, tag: &str) -> Result<u64, RpcError> {
    as_u64(
        &rpc.request("eth_getTransactionCount", json!([who, tag]))
            .await?,
    )
}

pub async fn balance(rpc: &dyn ChainRpc, who: Address) -> Result<U256, RpcError> {
    de(rpc
        .request("eth_getBalance", json!([who, "latest"]))
        .await?)
}

pub async fn send_raw(rpc: &dyn ChainRpc, raw: &Bytes) -> Result<B256, RpcError> {
    de(rpc.request("eth_sendRawTransaction", json!([raw])).await?)
}

pub async fn receipt(rpc: &dyn ChainRpc, hash: B256) -> Result<Option<Receipt>, RpcError> {
    let v = rpc
        .request("eth_getTransactionReceipt", json!([hash]))
        .await?;
    if v.is_null() {
        Ok(None)
    } else {
        de(v).map(Some)
    }
}

pub async fn eth_call(
    rpc: &dyn ChainRpc,
    from: Option<Address>,
    to: Address,
    data: &Bytes,
) -> Result<Bytes, RpcError> {
    let mut call = json!({"to": to, "data": data});
    if let Some(f) = from {
        call["from"] = json!(f);
    }
    de(rpc.request("eth_call", json!([call, "latest"])).await?)
}

pub async fn estimate_gas(
    rpc: &dyn ChainRpc,
    from: Address,
    to: Address,
    data: &Bytes,
) -> Result<u64, RpcError> {
    as_u64(
        &rpc.request(
            "eth_estimateGas",
            json!([{"from": from, "to": to, "data": data}]),
        )
        .await?,
    )
}

pub async fn get_logs(
    rpc: &dyn ChainRpc,
    addresses: &[Address],
    from: u64,
    to: u64,
) -> Result<Vec<Log>, RpcError> {
    let f = json!([{"address": addresses, "fromBlock": format!("0x{from:x}"), "toBlock": format!("0x{to:x}")}]);
    de(rpc.request("eth_getLogs", f).await?)
}

pub async fn head_by_tag(rpc: &dyn ChainRpc, tag: &str) -> Result<Option<Head>, RpcError> {
    let v = rpc
        .request("eth_getBlockByNumber", json!([tag, false]))
        .await?;
    if v.is_null() {
        Ok(None)
    } else {
        de(v).map(Some)
    }
}

pub async fn raw_tx_by_hash(rpc: &dyn ChainRpc, hash: B256) -> Result<Option<Bytes>, RpcError> {
    let v = rpc
        .request("eth_getRawTransactionByHash", json!([hash]))
        .await?;
    if v.is_null() {
        Ok(None)
    } else {
        de(v).map(Some)
    }
}
