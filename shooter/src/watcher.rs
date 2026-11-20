//! Chain event intake: several WebSocket upstreams race, a merger de-duplicates, and gaps after a
//! disconnect are filled from HTTP `eth_getLogs`.
//!
//! What a public `newPendingTransactions` feed can show: transactions gossiped on the public
//! mempool of that node. It cannot show transactions sent privately to builders or bundles.
use crate::config::PendingRule;
use crate::launch_adapter::rule_matches;
use crate::rpc::{self, redact_url, ChainRpc};
use crate::types::{now_ms, ChainEvent, Head, Log, PendingTx};
use alloy::primitives::{Address, B256};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Key {
    Head(B256),
    Log(B256, u64),
    Removed(B256, u64),
    Pending(B256),
}

/// First-seen filter with bounded memory.
pub struct Merger {
    seen: HashSet<Key>,
    order: VecDeque<Key>,
    cap: usize,
}

impl Merger {
    pub fn new(cap: usize) -> Self {
        Self {
            seen: HashSet::new(),
            order: VecDeque::new(),
            cap,
        }
    }

    /// Returns the event only the first time it is seen, whichever upstream delivers it.
    pub fn accept(&mut self, ev: ChainEvent) -> Option<ChainEvent> {
        let key = match &ev {
            ChainEvent::Head { head, .. } => Key::Head(head.hash),
            ChainEvent::Log { log, .. } => Key::Log(log.block_hash, log.log_index),
            ChainEvent::LogRemoved { log, .. } => Key::Removed(log.block_hash, log.log_index),
            ChainEvent::Pending { tx, .. } => Key::Pending(tx.hash),
        };
        // A log that comes back after a reorg (same block hash re-included) must be deliverable again.
        if let ChainEvent::LogRemoved { log, .. } = &ev {
            self.seen.remove(&Key::Log(log.block_hash, log.log_index));
        }
        if let ChainEvent::Log { log, .. } = &ev {
            self.seen
                .remove(&Key::Removed(log.block_hash, log.log_index));
        }
        if !self.seen.insert(key.clone()) {
            return None;
        }
        self.order.push_back(key);
        while self.order.len() > self.cap {
            if let Some(old) = self.order.pop_front() {
                self.seen.remove(&old);
            }
        }
        Some(ev)
    }
}

/// Re-reads logs for `[from, to]`. Used after reconnects and at startup; duplicates are dropped by
/// the merger, so over-reading is safe.
pub async fn backfill(
    rpc: &dyn ChainRpc,
    addresses: &[Address],
    from: u64,
    to: u64,
    source: usize,
) -> Result<Vec<ChainEvent>, rpc::RpcError> {
    let logs = rpc::get_logs(rpc, addresses, from, to).await?;
    Ok(logs
        .into_iter()
        .map(|log| {
            if log.removed {
                ChainEvent::LogRemoved { log, source }
            } else {
                ChainEvent::Log {
                    log,
                    source,
                    seen_ms: now_ms(),
                }
            }
        })
        .collect())
}

#[derive(Clone)]
pub struct WsSource {
    pub url: String,
    pub index: usize,
    pub addresses: Vec<Address>,
    /// Non-empty enables the pending subscription on this upstream.
    pub pending_rules: Vec<PendingRule>,
    pub backfill_blocks: u64,
}

/// Parses one subscription notification payload into an event. `kind` is what we subscribed to.
pub fn parse_notification(kind: &str, result: &Value, source: usize) -> Option<ChainEvent> {
    match kind {
        "newHeads" => serde_json::from_value::<Head>(result.clone())
            .ok()
            .map(|head| ChainEvent::Head {
                head,
                source,
                seen_ms: now_ms(),
            }),
        "logs" => serde_json::from_value::<Log>(result.clone())
            .ok()
            .map(|log| {
                if log.removed {
                    ChainEvent::LogRemoved { log, source }
                } else {
                    ChainEvent::Log {
                        log,
                        source,
                        seen_ms: now_ms(),
                    }
                }
            }),
        // Nodes that only push hashes (a JSON string) are not usable for target matching.
        "newPendingTransactions" => serde_json::from_value::<PendingTx>(result.clone())
            .ok()
            .map(|tx| ChainEvent::Pending {
                tx,
                source,
                seen_ms: now_ms(),
            }),
        _ => None,
    }
}

/// Runs one upstream forever: connect, subscribe, forward, reconnect with backoff and backfill.
pub async fn run_ws(
    src: WsSource,
    http: Arc<dyn ChainRpc>,
    last_block: Arc<AtomicU64>,
    out: mpsc::Sender<ChainEvent>,
) {
    let mut backoff = Duration::from_millis(250);
    loop {
        match session(&src, &http, &last_block, &out).await {
            Ok(()) => tracing::warn!(source = %redact_url(&src.url), "ws closed"),
            Err(e) => tracing::warn!(source = %redact_url(&src.url), error = %e, "ws error"),
        }
        if out.is_closed() {
            return;
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(5));
    }
}

async fn session(
    src: &WsSource,
    http: &Arc<dyn ChainRpc>,
    last_block: &Arc<AtomicU64>,
    out: &mpsc::Sender<ChainEvent>,
) -> anyhow::Result<()> {
    let (mut ws, _) = tokio_tungstenite::connect_async(&src.url).await?;
    let mut subs: Vec<(&str, Value)> = vec![
        ("newHeads", json!(["newHeads"])),
        ("logs", json!(["logs", {"address": src.addresses}])),
    ];
    if !src.pending_rules.is_empty() {
        subs.push((
            "newPendingTransactions",
            json!(["newPendingTransactions", true]),
        ));
    }
    for (i, (_, params)) in subs.iter().enumerate() {
        ws.send(Message::text(
            json!({"jsonrpc":"2.0","id": i + 1,"method":"eth_subscribe","params": params})
                .to_string(),
        ))
        .await?;
    }
    // Fill the gap between the last block we saw and now, before trusting the live stream alone.
    let seen = last_block.load(Ordering::Relaxed);
    if seen > 0 {
        let tip = rpc::block_number(http.as_ref()).await?;
        for ev in backfill(
            http.as_ref(),
            &src.addresses,
            seen.saturating_sub(src.backfill_blocks),
            tip,
            src.index,
        )
        .await?
        {
            out.send(ev).await?;
        }
    }
    let mut ids: Vec<Option<String>> = vec![None; subs.len()];
    while let Some(msg) = ws.next().await {
        let text = match msg? {
            Message::Text(t) => t.to_string(),
            Message::Ping(p) => {
                ws.send(Message::Pong(p)).await?;
                continue;
            }
            Message::Close(_) => break,
            _ => continue,
        };
        let v: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if let (Some(id), Some(res)) = (
            v.get("id").and_then(Value::as_u64),
            v.get("result").and_then(Value::as_str),
        ) {
            if let Some(slot) = ids.get_mut(id as usize - 1) {
                *slot = Some(res.to_string());
            }
            continue;
        }
        if let Some(err) = v.get("error") {
            anyhow::bail!("subscribe rejected: {err}");
        }
        let Some(params) = v.get("params") else {
            continue;
        };
        let sub = params.get("subscription").and_then(Value::as_str);
        let Some(pos) = ids
            .iter()
            .position(|s| s.as_deref() == sub && sub.is_some())
        else {
            continue;
        };
        let Some(ev) = parse_notification(subs[pos].0, &params["result"], src.index) else {
            continue;
        };
        let ev = match ev {
            ChainEvent::Head {
                head,
                source,
                seen_ms,
            } => {
                last_block.fetch_max(head.number, Ordering::Relaxed);
                ChainEvent::Head {
                    head,
                    source,
                    seen_ms,
                }
            }
            ChainEvent::Pending {
                mut tx,
                source,
                seen_ms,
            } => {
                if !src.pending_rules.iter().any(|r| rule_matches(r, &tx)) {
                    continue;
                }
                // The signed bytes are required to place the target inside a bundle.
                tx.raw = rpc::raw_tx_by_hash(http.as_ref(), tx.hash)
                    .await
                    .ok()
                    .flatten();
                ChainEvent::Pending {
                    tx,
                    source,
                    seen_ms,
                }
            }
            other => other,
        };
        out.send(ev).await?;
    }
    Ok(())
}
