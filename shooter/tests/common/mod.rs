//! In-memory chain used by the engine tests. It models what matters for the task manager:
//! account nonce, a mempool, block production with a controllable launch position, receipts with
//! real gas accounting, reorgs and foreign nonce use. It is driven through the same `ChainRpc`
//! trait and the same signed raw transactions as a real node.
#![allow(dead_code)]
use alloy::consensus::{Transaction, TxEnvelope};
use alloy::eips::eip2718::Decodable2718;
use alloy::primitives::{address, b256, Address, Bytes, B256, U256};
use alloy::sol_types::{SolEvent, SolValue};
use async_trait::async_trait;
use serde_json::{json, Value};
use shooter::budget::{PricePolicy, PriceQuote};
use shooter::config::{ChannelKind, Mode, TaskSpec};
use shooter::dex_adapter::{v2_pair_address, Sync, TaskExecuted};
use shooter::engine::{Engine, EngineOpts};
use shooter::rpc::{ChainRpc, RpcError};
use shooter::signer::ExecSigner;
use shooter::store::Store;
use shooter::submitter::{Channel, JsonRpcChannel};
use shooter::types::{ChainEvent, Head, Log};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const KEY: &str = "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
pub const FACTORY: Address = address!("cA143Ce32Fe78f1f7019d7d551a6402fC5350c73");
pub const INIT: B256 = b256!("00fb7f630766e6a796048ea87d01acd3068e8ff67d078148a3fa3f4a84f69bd5");
pub const WBNB: Address = address!("bb4CdB9CBd36B01bD1cBaEBF2De08d9173bc095c");
pub const TOKEN: Address = address!("EbE3069bC7B5F1602De421f7a1089759F15AB636");
pub const EXECUTOR_CONTRACT: Address = address!("00000000000000000000000000000000000000e1");
pub const PROBE: Address = address!("00000000000000000000000000000000000000f1");
pub const TASK: B256 = B256::repeat_byte(0x77);
// Receipt values measured with the real binary on the SLERF mainnet fork (scripts/fork_replay.py).
pub const GAS_FAIL_PRE: u64 = 46_231; // attempt that lands before the pool exists
pub const GAS_FAIL_POST: u64 = 30_000; // illustrative: attempt after the task is spent
pub const GAS_OK: u64 = 190_694; // first sale in a block after the launch block

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SendMode {
    Accept,
    /// The node takes the transaction but the client sees a timeout.
    TimeoutButAccept,
    Reject,
}

#[derive(Clone)]
struct MemTx {
    hash: B256,
    nonce: u64,
    gas_price: u128,
    raw: Bytes,
}

#[derive(Default)]
pub struct Inner {
    pub block: u64,
    pub nonce: u64,
    mempool: Vec<MemTx>,
    receipts: HashMap<B256, Value>,
    pub opened: bool,
    pub sold: bool,
    pub send_mode: Option<SendMode>,
    pub sim_ok: bool,
    pub probe_value: u64,
    pub finalized: u64,
    pub logs: Vec<Log>,
    pub calls: Vec<String>,
    pub bundles: Vec<Value>,
    pub accepted: Vec<B256>,
}

pub struct SimChain {
    pub i: Mutex<Inner>,
}

fn decode(raw: &Bytes) -> MemTx {
    let env = TxEnvelope::decode_2718(&mut &raw[..]).expect("valid signed tx");
    MemTx {
        hash: *env.tx_hash(),
        nonce: env.nonce(),
        gas_price: env.gas_price().unwrap_or_default(),
        raw: raw.clone(),
    }
}

impl SimChain {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            i: Mutex::new(Inner {
                block: 100,
                nonce: 5,
                sim_ok: true,
                ..Default::default()
            }),
        })
    }

    pub fn set(&self, f: impl FnOnce(&mut Inner)) {
        f(&mut self.i.lock().unwrap())
    }
    pub fn get<T>(&self, f: impl FnOnce(&Inner) -> T) -> T {
        f(&self.i.lock().unwrap())
    }
    pub fn mempool_len(&self) -> usize {
        self.get(|i| i.mempool.len())
    }
    pub fn count(&self, method: &str) -> usize {
        self.get(|i| i.calls.iter().filter(|c| c.as_str() == method).count())
    }

    /// Mines one block. `launch_at` puts the launch transaction at that index in this block; our
    /// transactions get consecutive indices starting at `our_first_index`.
    pub fn mine(&self, launch_at: Option<u64>, our_first_index: u64) -> ChainEvent {
        let mut i = self.i.lock().unwrap();
        i.block += 1;
        let block = i.block;
        let block_hash = B256::from(U256::from(block * 1000 + 7));
        let mut txs = std::mem::take(&mut i.mempool);
        txs.sort_by_key(|t| t.nonce);
        let mut idx = our_first_index;
        let mut left = vec![];
        if let Some(l) = launch_at {
            i.logs.push(liquidity_log(block, l));
        }
        for t in txs {
            if t.nonce != i.nonce {
                if t.nonce > i.nonce {
                    left.push(t);
                }
                continue;
            }
            if launch_at.is_some_and(|l| idx > l) {
                i.opened = true;
            }
            let (status, gas, logs) = if i.opened && !i.sold {
                i.sold = true;
                let ev = json!({"address": EXECUTOR_CONTRACT, "topics": [TaskExecuted::SIGNATURE_HASH, TASK], "data": Bytes::from((U256::from(500u64), U256::from(500u64), U256::from(44u64)).abi_encode()),
                    "blockNumber": format!("0x{block:x}"), "blockHash": block_hash, "transactionHash": t.hash, "transactionIndex": format!("0x{idx:x}"), "logIndex": "0x0", "removed": false});
                (1, GAS_OK, vec![ev])
            } else {
                (
                    0,
                    if i.opened {
                        GAS_FAIL_POST
                    } else {
                        GAS_FAIL_PRE
                    },
                    vec![],
                )
            };
            i.receipts.insert(t.hash, json!({"transactionHash": t.hash, "blockNumber": format!("0x{block:x}"), "blockHash": block_hash, "transactionIndex": format!("0x{idx:x}"),
                "gasUsed": format!("0x{gas:x}"), "status": format!("0x{status:x}"), "effectiveGasPrice": format!("0x{:x}", t.gas_price), "logs": logs}));
            i.nonce += 1;
            idx += 1;
        }
        if launch_at.is_some() {
            i.opened = true;
        }
        i.mempool = left;
        ChainEvent::Head {
            head: Head {
                number: block,
                hash: block_hash,
                parent_hash: B256::ZERO,
                timestamp: block,
            },
            source: 0,
            seen_ms: 0,
        }
    }

    /// Drops the receipt of `hash` and returns the transaction to the mempool, as after a reorg.
    pub fn reorg_out(&self, hash: B256, raw: &Bytes) {
        let mut i = self.i.lock().unwrap();
        let r = i.receipts.remove(&hash).expect("was included");
        if r["status"] == "0x1" {
            i.sold = false;
        }
        i.nonce -= 1;
        i.mempool.push(decode(raw));
    }

    /// Another transaction (not ours) consumes the executor's current nonce.
    pub fn foreign_nonce_use(&self) {
        let mut i = self.i.lock().unwrap();
        let n = i.nonce;
        i.mempool.retain(|t| t.nonce != n);
        i.nonce += 1;
    }

    pub fn raw_of(&self, hash: B256) -> Option<Bytes> {
        self.get(|i| {
            i.mempool
                .iter()
                .find(|t| t.hash == hash)
                .map(|t| t.raw.clone())
        })
    }
}

pub fn liquidity_log(block: u64, tx_index: u64) -> Log {
    Log {
        address: v2_pair_address(FACTORY, INIT, TOKEN, WBNB),
        topics: vec![Sync::SIGNATURE_HASH],
        data: (U256::from(18u64), U256::from(200u64)).abi_encode().into(),
        block_number: block,
        block_hash: B256::from(U256::from(block * 1000 + 7)),
        transaction_hash: B256::from(U256::from(block * 1000 + 1)),
        transaction_index: tx_index,
        log_index: 3,
        removed: false,
    }
}

pub fn log_event(log: Log, source: usize) -> ChainEvent {
    ChainEvent::Log {
        log,
        source,
        seen_ms: 1,
    }
}

#[async_trait]
impl ChainRpc for SimChain {
    async fn request(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let mut i = self.i.lock().unwrap();
        i.calls.push(method.to_string());
        match method {
            "eth_blockNumber" => Ok(json!(format!("0x{:x}", i.block))),
            "eth_getTransactionCount" => {
                let mut n = i.nonce;
                if params[1] == "pending" {
                    let mut ns: Vec<u64> = i.mempool.iter().map(|t| t.nonce).collect();
                    ns.sort();
                    for x in ns {
                        if x == n {
                            n += 1;
                        }
                    }
                }
                Ok(json!(format!("0x{n:x}")))
            }
            "eth_sendRawTransaction" | "eth_sendPrivateTransaction" => {
                let raw: Bytes = serde_json::from_value(params[0].clone()).unwrap();
                let tx = decode(&raw);
                match i.send_mode.unwrap_or(SendMode::Accept) {
                    SendMode::Reject => Err(RpcError::Rpc {
                        code: -32000,
                        message: "rejected".into(),
                        data: None,
                    }),
                    mode => {
                        if tx.nonce >= i.nonce && !i.mempool.iter().any(|t| t.hash == tx.hash) {
                            i.accepted.push(tx.hash);
                            i.mempool.push(tx.clone());
                        }
                        if mode == SendMode::TimeoutButAccept {
                            Err(RpcError::Timeout)
                        } else {
                            Ok(json!(tx.hash))
                        }
                    }
                }
            }
            "eth_sendBundle" => {
                i.bundles.push(params[0].clone());
                Ok(json!("0xbundle"))
            }
            "eth_getTransactionReceipt" => {
                let h: B256 = serde_json::from_value(params[0].clone()).unwrap();
                Ok(i.receipts.get(&h).cloned().unwrap_or(Value::Null))
            }
            "eth_call" => {
                let to: Address = serde_json::from_value(params[0]["to"].clone()).unwrap();
                if to == PROBE {
                    Ok(json!(Bytes::from(U256::from(i.probe_value).abi_encode())))
                } else if i.sim_ok {
                    Ok(json!("0x"))
                } else {
                    Err(RpcError::Rpc {
                        code: 3,
                        message: "execution reverted".into(),
                        data: None,
                    })
                }
            }
            "eth_getBlockByNumber" => Ok(
                json!({"number": format!("0x{:x}", i.finalized), "hash": B256::ZERO, "parentHash": B256::ZERO, "timestamp": "0x1"}),
            ),
            "eth_getLogs" => {
                let from = u64::from_str_radix(
                    params[0]["fromBlock"]
                        .as_str()
                        .unwrap()
                        .trim_start_matches("0x"),
                    16,
                )
                .unwrap();
                let to = u64::from_str_radix(
                    params[0]["toBlock"]
                        .as_str()
                        .unwrap()
                        .trim_start_matches("0x"),
                    16,
                )
                .unwrap();
                let logs: Vec<Value> = i.logs.iter().filter(|l| l.block_number >= from && l.block_number <= to).map(|l| json!({"address": l.address, "topics": l.topics, "data": l.data, "blockNumber": format!("0x{:x}", l.block_number), "blockHash": l.block_hash, "transactionHash": l.transaction_hash, "transactionIndex": format!("0x{:x}", l.transaction_index), "logIndex": format!("0x{:x}", l.log_index), "removed": false})).collect();
                Ok(json!(logs))
            }
            other => Err(RpcError::Rpc {
                code: -32601,
                message: format!("method {other} not supported by SimChain"),
                data: None,
            }),
        }
    }
}

pub fn spec(mode: Mode) -> TaskSpec {
    TaskSpec {
        task_id: TASK,
        executor_contract: EXECUTOR_CONTRACT,
        owner: address!("00000000000000000000000000000000000000b0"),
        token_in: TOKEN,
        token_out: WBNB,
        factory: FACTORY,
        pair_init_code_hash: INIT,
        pair_fee_bps: 25,
        amount_in: U256::from(500u64),
        min_out: U256::from(40u64),
        mode,
        gas_limit: 300_000,
        gas_price_wei: 50_000_000,
        channels: vec!["main".into()],
        allow_public_with_private: false,
        max_attempts: 1,
        window_blocks: 3,
        require_sim_ok: false,
        spray_per_block: 1,
        arm_probe: None,
        strict_targets: vec![],
    }
}

pub const POLICY: PricePolicy = PricePolicy {
    max_age_s: 30,
    min_price_micros: 100_000_000,
    max_price_micros: 5_000_000_000,
    buffer_bps: 500,
};
pub const NOW_S: u64 = 1_000_000;

pub fn quote() -> Option<PriceQuote> {
    Some(PriceQuote {
        price_micros: 600_000_000,
        fetched_at_s: NOW_S - 5,
    })
}

pub fn opts() -> EngineOpts {
    EngineOpts {
        dry_run: false,
        submit_timeout: Duration::from_millis(500),
        has_pending_source: true,
        usd_micros: 500_000,
        price_policy: POLICY,
        include_init_costs: false,
    }
}

pub fn channel(sim: &Arc<SimChain>, kind: ChannelKind) -> Vec<Arc<dyn Channel>> {
    vec![Arc::new(JsonRpcChannel::new(
        "main",
        kind,
        sim.clone(),
        Duration::ZERO,
    ))]
}

pub async fn engine(
    sim: &Arc<SimChain>,
    store: &Arc<Store>,
    o: EngineOpts,
    kind: ChannelKind,
) -> Engine {
    let signer = Arc::new(ExecSigner::from_secret(KEY, 56).unwrap());
    Engine::new(o, sim.clone(), store.clone(), signer, channel(sim, kind))
        .await
        .unwrap()
}
