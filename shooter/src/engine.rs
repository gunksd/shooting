//! Task manager: the single owner of task state, the executor nonce and the budget.
//! It is driven only by `on_event`, so competing triggers from several upstreams are serialized
//! here, and the database compare-and-set guards against a second process or a restart.
use crate::budget::{cap_from_usd, Budget, PricePolicy, PriceQuote};
use crate::config::{Mode, TaskSpec};
use crate::dex_adapter::{DexAdapter, PancakeV2};
use crate::launch_adapter::{LaunchAdapter, LaunchSignal, TargetSet, V2Launch};
use crate::planner::Planner;
use crate::receipt_tracker::{classify, placement, Outcome, Placement};
use crate::rpc::{self, ChainRpc, RpcError};
use crate::signer::{ExecSigner, NonceManager, SignedTx};
use crate::store::{AttemptRow, Store};
use crate::submitter::{broadcast_backrun, broadcast_tx, Channel};
use crate::types::{now_ms, ChainEvent, PendingTx};
use alloy::primitives::{Address, Bytes, B256, U256};
use anyhow::{bail, Result};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct EngineOpts {
    pub dry_run: bool,
    pub submit_timeout: Duration,
    /// A pending-transaction upstream is configured and running.
    pub has_pending_source: bool,
    pub usd_micros: u64,
    pub price_policy: PricePolicy,
    pub include_init_costs: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum St {
    Ready,
    Armed,
    Triggered,
    Confirmed,
    Stopped,
    Expired,
    Cancelled,
}

impl St {
    pub fn as_str(self) -> &'static str {
        match self {
            St::Ready => "ready",
            St::Armed => "armed",
            St::Triggered => "triggered",
            St::Confirmed => "confirmed",
            St::Stopped => "stopped",
            St::Expired => "expired",
            St::Cancelled => "cancelled",
        }
    }
    fn parse(s: &str) -> St {
        match s {
            "armed" => St::Armed,
            "triggered" => St::Triggered,
            "confirmed" => St::Confirmed,
            "stopped" => St::Stopped,
            "expired" => St::Expired,
            "cancelled" => St::Cancelled,
            _ => St::Ready,
        }
    }
    fn open(self) -> bool {
        matches!(self, St::Ready | St::Armed | St::Triggered)
    }
}

pub struct TaskRt {
    pub spec: TaskSpec,
    pub dex: PancakeV2,
    pub launch: V2Launch,
    targets: TargetSet,
    pub budget: Budget,
    pub state: St,
    pub reason: String,
    pub armed_block: Option<u64>,
    pub trigger_block: Option<u64>,
    /// Bundle expiry for strict mode.
    max_block: Option<u64>,
    pub open: Option<(u64, u64)>,
    pub sent: u32,
    wait_sim: bool,
    wait_retry: bool,
    pub outcome: Option<Outcome>,
    pub placement: Option<Placement>,
}

struct Inflight {
    tx: SignedTx,
    task: B256,
    misses: u32,
    /// Included and settled, kept until finalized to catch reorgs.
    included: Option<Outcome>,
}

pub struct Engine {
    opts: EngineOpts,
    rpc: Arc<dyn ChainRpc>,
    store: Arc<Store>,
    signer: Arc<ExecSigner>,
    nonces: NonceManager,
    planner: Planner,
    channels: Vec<Arc<dyn Channel>>,
    pub tasks: Vec<TaskRt>,
    inflight: Vec<Inflight>,
    pub head: u64,
}

impl Engine {
    pub async fn new(
        opts: EngineOpts,
        rpc: Arc<dyn ChainRpc>,
        store: Arc<Store>,
        signer: Arc<ExecSigner>,
        channels: Vec<Arc<dyn Channel>>,
    ) -> Result<Self> {
        let nonce = rpc::nonce_at(rpc.as_ref(), signer.address(), "pending").await?;
        let head = rpc::block_number(rpc.as_ref()).await?;
        Ok(Self {
            opts,
            rpc,
            store,
            signer,
            nonces: NonceManager::new(nonce),
            planner: Planner::default(),
            channels,
            tasks: vec![],
            inflight: vec![],
            head,
        })
    }

    fn chans_for(&self, spec: &TaskSpec) -> Vec<Arc<dyn Channel>> {
        self.channels
            .iter()
            .filter(|c| spec.channels.iter().any(|n| n == c.name()))
            .cloned()
            .collect()
    }

    /// Registers a task. A new task needs a fresh, sane price quote to fix its BNB cap; a task
    /// already in the database keeps the cap and spending it had.
    pub fn add_task(
        &mut self,
        spec: TaskSpec,
        quote: Option<PriceQuote>,
        now_s: u64,
    ) -> Result<()> {
        let chans = self.chans_for(&spec);
        if chans.len() != spec.channels.len() {
            bail!(
                "task {}: a configured channel is not available",
                spec.task_id
            );
        }
        if spec.mode == Mode::StrictSameBlock {
            if !self.opts.has_pending_source {
                bail!("task {}: strict_same_block refused: no pending-transaction source is configured", spec.task_id);
            }
            if !chans
                .iter()
                .any(|c| c.caps().bundle_foreign_target && c.caps().max_block_number)
            {
                bail!("task {}: strict_same_block refused: no selected channel can bundle behind a foreign target", spec.task_id);
            }
        }
        let (cap, spent, state, sent, armed) = match self.store.task(spec.task_id)? {
            Some(row) => (
                row.cap_wei,
                row.spent_wei,
                St::parse(&row.state),
                row.attempts,
                row.armed_block,
            ),
            None => {
                let q = quote.ok_or_else(|| {
                    anyhow::anyhow!(
                        "task {}: no BNB price available, refusing to arm",
                        spec.task_id
                    )
                })?;
                let cap = cap_from_usd(self.opts.usd_micros, q, now_s, &self.opts.price_policy)?;
                let init = if self.opts.include_init_costs {
                    self.store.init_cost_total(spec.task_id)?
                } else {
                    0
                };
                self.store.upsert_task(
                    spec.task_id,
                    &serde_json::to_string(&spec)?,
                    cap,
                    now_ms(),
                )?;
                self.store.set_spent(spec.task_id, init)?;
                (cap, init, St::Ready, 0, None)
            }
        };
        let dex = PancakeV2 {
            factory: spec.factory,
            init_code_hash: spec.pair_init_code_hash,
            token_in: spec.token_in,
            token_out: spec.token_out,
            fee_bps: spec.pair_fee_bps,
        };
        let launch = V2Launch::new(spec.factory, dex.pool(), spec.token_in, spec.token_out);
        let targets = TargetSet::new(spec.strict_targets.clone());
        self.tasks.push(TaskRt {
            spec,
            dex,
            launch,
            targets,
            budget: Budget::new(cap, spent),
            state,
            reason: String::new(),
            armed_block: armed,
            trigger_block: None,
            max_block: None,
            open: None,
            sent,
            wait_sim: false,
            wait_retry: false,
            outcome: None,
            placement: None,
        });
        Ok(())
    }

    /// Reloads transactions that may still land (sent, unknown, or signed right before a crash) and
    /// re-reserves their worst-case cost. Call after all tasks were added.
    pub async fn recover(&mut self) -> Result<()> {
        for a in self
            .store
            .attempts_in_state(&["signed", "sent", "unknown"])?
        {
            let tx = SignedTx {
                hash: a.tx_hash,
                raw: a.raw_tx.clone().into(),
                nonce: a.nonce,
                gas_limit: a.gas_limit,
                gas_price: a.gas_price,
                max_cost: a.max_cost,
            };
            if let Some(t) = self.tasks.iter_mut().find(|t| t.spec.task_id == a.task_id) {
                t.budget.reopen(tx.nonce, tx.hash, tx.max_cost, 0);
                t.trigger_block.get_or_insert(a.submitted_block);
            }
            self.inflight.push(Inflight {
                tx,
                task: a.task_id,
                misses: 0,
                included: None,
            });
        }
        let chain = rpc::nonce_at(self.rpc.as_ref(), self.signer.address(), "latest").await?;
        self.nonces
            .reconcile(chain, self.inflight.iter().map(|f| f.tx.nonce).max());
        Ok(())
    }

    pub fn addresses(&self) -> Vec<Address> {
        let mut v: Vec<Address> = self
            .tasks
            .iter()
            .flat_map(|t| t.launch.addresses())
            .collect();
        v.sort();
        v.dedup();
        v
    }

    pub fn task(&self, id: B256) -> Option<&TaskRt> {
        self.tasks.iter().find(|t| t.spec.task_id == id)
    }

    fn set_state(&mut self, ti: usize, to: St, reason: &str) -> Result<()> {
        let t = &mut self.tasks[ti];
        if t.state == to {
            return Ok(());
        }
        self.store
            .transition(t.spec.task_id, &[t.state.as_str()], to.as_str(), now_ms())?;
        tracing::info!(task = %t.spec.task_id, from = t.state.as_str(), to = to.as_str(), reason, "task state");
        t.state = to;
        t.reason = reason.to_string();
        Ok(())
    }

    /// Local stop. It cannot recall transactions already handed to a channel.
    pub fn cancel(&mut self, id: B256) -> Result<()> {
        if let Some(ti) = self
            .tasks
            .iter()
            .position(|t| t.spec.task_id == id && t.state.open())
        {
            self.set_state(ti, St::Cancelled, "operator")?;
        }
        Ok(())
    }

    /// Manual arming for pre_armed_spray.
    pub fn arm(&mut self, id: B256) -> Result<()> {
        if let Some(ti) = self.tasks.iter().position(|t| {
            t.spec.task_id == id && t.state == St::Ready && t.spec.mode == Mode::PreArmedSpray
        }) {
            self.tasks[ti].armed_block = Some(self.head);
            self.store.set_armed(id, Some(self.head))?;
            self.set_state(ti, St::Armed, "armed")?;
        }
        Ok(())
    }

    pub async fn on_event(&mut self, ev: ChainEvent) -> Result<()> {
        match ev {
            ChainEvent::Head { head, .. } => {
                self.head = self.head.max(head.number);
                self.track().await?;
                for ti in 0..self.tasks.len() {
                    self.tick(ti).await?;
                }
            }
            ChainEvent::Log { log, seen_ms, .. } => {
                for ti in 0..self.tasks.len() {
                    match self.tasks[ti].launch.on_log(&log) {
                        Some(LaunchSignal::Liquidity {
                            block,
                            tx_index,
                            tx_hash,
                        }) => {
                            let id = self.tasks[ti].spec.task_id;
                            self.tasks[ti].open = Some((block, tx_index));
                            self.store.set_open(id, block, tx_index, tx_hash)?;
                            self.store.metric(
                                id,
                                "launch_event_seen_ms",
                                seen_ms as i64,
                                now_ms(),
                            )?;
                            self.record_placement(ti)?;
                            if self.tasks[ti].spec.mode != Mode::StrictSameBlock {
                                self.try_fire(ti, seen_ms).await?;
                            }
                        }
                        Some(LaunchSignal::PoolCreated { block }) => {
                            tracing::info!(task = %self.tasks[ti].spec.task_id, block, "pool created")
                        }
                        _ => {}
                    }
                }
            }
            ChainEvent::LogRemoved { log, .. } => {
                for t in self.tasks.iter_mut() {
                    if let Some(LaunchSignal::Reverted { tx_hash }) = t.launch.on_removed(&log) {
                        tracing::warn!(task = %t.spec.task_id, %tx_hash, "launch log removed by reorg");
                        t.open = None;
                        t.placement = None;
                        t.wait_sim = false;
                    }
                }
            }
            ChainEvent::Pending { tx, seen_ms, .. } => self.on_pending(tx, seen_ms).await?,
        }
        Ok(())
    }

    async fn on_pending(&mut self, tx: PendingTx, seen_ms: u64) -> Result<()> {
        for ti in 0..self.tasks.len() {
            let t = &mut self.tasks[ti];
            if t.spec.mode != Mode::StrictSameBlock || t.state != St::Ready {
                continue;
            }
            let Some(all) = t.targets.offer(&tx) else {
                continue;
            };
            let raws: Vec<Bytes> = all.iter().filter_map(|p| p.raw.clone()).collect();
            if !self
                .store
                .transition(t.spec.task_id, &["ready"], "triggered", now_ms())?
            {
                continue;
            }
            t.state = St::Triggered;
            t.trigger_block = Some(self.head);
            self.send_attempt(ti, Some(raws), seen_ms).await?;
        }
        Ok(())
    }

    /// First trigger for next-block and spray tasks. The database compare-and-set makes this
    /// effective at most once per task no matter how many upstreams report the launch.
    async fn try_fire(&mut self, ti: usize, seen_ms: u64) -> Result<()> {
        let (spec, state) = (self.tasks[ti].spec.clone(), self.tasks[ti].state);
        if !matches!(state, St::Ready | St::Armed) {
            return Ok(());
        }
        if spec.require_sim_ok && !self.simulate(&spec).await {
            self.tasks[ti].wait_sim = true;
            return Ok(());
        }
        self.tasks[ti].wait_sim = false;
        if !self
            .store
            .transition(spec.task_id, &["ready", "armed"], "triggered", now_ms())?
        {
            return Ok(());
        }
        self.tasks[ti].state = St::Triggered;
        self.tasks[ti].trigger_block = Some(self.head);
        self.send_attempt(ti, None, seen_ms).await?;
        Ok(())
    }

    async fn simulate(&self, spec: &TaskSpec) -> bool {
        let dex = PancakeV2 {
            factory: spec.factory,
            init_code_hash: spec.pair_init_code_hash,
            token_in: spec.token_in,
            token_out: spec.token_out,
            fee_bps: spec.pair_fee_bps,
        };
        let data = dex.sell_calldata(spec.task_id, spec.amount_in, spec.min_out);
        rpc::eth_call(
            self.rpc.as_ref(),
            Some(self.signer.address()),
            spec.executor_contract,
            &data,
        )
        .await
        .is_ok()
    }

    /// Signs, reserves budget, persists, then hands the transaction to the channels.
    /// Returns true when the transaction may be on its way.
    async fn send_attempt(
        &mut self,
        ti: usize,
        targets: Option<Vec<Bytes>>,
        seen_ms: u64,
    ) -> Result<bool> {
        let spec = self.tasks[ti].spec.clone();
        let id = spec.task_id;
        if self.tasks[ti].sent >= spec.max_attempts {
            return Ok(false);
        }
        let t0 = Instant::now();
        let nonce = self.nonces.peek();
        let tx = self
            .planner
            .presign(&self.signer, &spec, &self.tasks[ti].dex, nonce)?;
        let sign_us = t0.elapsed().as_micros() as i64;

        // Worst case is known before anything leaves the process.
        if let Err(e) = self.tasks[ti]
            .budget
            .try_reserve(nonce, tx.hash, tx.max_cost)
        {
            self.set_state(ti, St::Stopped, &format!("{e}"))?;
            return Ok(false);
        }
        let chans = self.chans_for(&spec);
        self.nonces.take();
        self.tasks[ti].sent += 1;
        let names = spec.channels.join(",");
        let row = AttemptRow {
            tx_hash: tx.hash,
            task_id: id,
            nonce,
            raw_tx: tx.raw.to_vec(),
            gas_limit: tx.gas_limit,
            gas_price: tx.gas_price,
            max_cost: tx.max_cost,
            state: if self.opts.dry_run {
                "dry_run"
            } else {
                "signed"
            }
            .into(),
            channels: names,
            submitted_block: self.head,
        };
        self.store.insert_attempt(&row, now_ms())?;
        self.store.metric(id, "sign_us", sign_us, now_ms())?;
        if seen_ms > 0 {
            self.store.metric(
                id,
                "decision_ms",
                now_ms().saturating_sub(seen_ms) as i64,
                now_ms(),
            )?;
        }

        let undo = |e: &mut Engine| {
            e.tasks[ti].budget.release_unsent(nonce, tx.hash);
            e.nonces = NonceManager::new(nonce);
        };
        if self.opts.dry_run {
            tracing::info!(task = %id, hash = %tx.hash, nonce, max_cost_wei = %tx.max_cost, "dry-run: transaction signed, not broadcast");
            undo(self);
            return Ok(false);
        }

        let max_block = self.head + spec.window_blocks;
        let acks = match &targets {
            None => broadcast_tx(&chans, &tx.raw, tx.gas_price, self.opts.submit_timeout).await,
            Some(t) => {
                broadcast_backrun(
                    &chans,
                    t,
                    &tx.raw,
                    tx.gas_price,
                    max_block,
                    self.opts.submit_timeout,
                )
                .await
            }
        };
        let acks = match acks {
            Ok(a) => a,
            Err(e) => {
                // Nothing was sent: capability or channel-minimum refusal. No fallback to another path.
                undo(self);
                self.store.finish_attempt(
                    tx.hash,
                    "not_sent",
                    None,
                    None,
                    None,
                    None,
                    0,
                    &e.to_string(),
                )?;
                self.set_state(ti, St::Stopped, &e.to_string())?;
                return Ok(false);
            }
        };
        for a in &acks {
            self.store.metric(
                id,
                &format!("submit_ms:{}", a.channel),
                a.elapsed_ms as i64,
                now_ms(),
            )?;
            tracing::info!(task = %id, channel = %a.channel, ms = a.elapsed_ms, accepted = a.result.is_ok(), "channel response (acceptance is not inclusion)");
        }
        let detail = acks
            .iter()
            .map(|a| {
                format!(
                    "{}:{}",
                    a.channel,
                    match &a.result {
                        Ok(_) => "accepted".to_string(),
                        Err(e) => e.to_string(),
                    }
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        // Only an explicit JSON-RPC error from every channel proves it went nowhere. A timeout or a
        // transport error leaves the outcome open, so the transaction stays tracked and reserved.
        if acks
            .iter()
            .all(|a| matches!(a.result, Err(RpcError::Rpc { .. })))
        {
            undo(self);
            self.store
                .finish_attempt(tx.hash, "rejected", None, None, None, None, 0, &detail)?;
            self.tasks[ti].wait_retry = true;
            return Ok(false);
        }
        let state = if acks.iter().any(|a| a.result.is_ok()) {
            "sent"
        } else {
            "unknown"
        };
        self.store
            .finish_attempt(tx.hash, state, None, None, None, None, 0, &detail)?;
        if targets.is_some() {
            self.tasks[ti].max_block = Some(max_block);
        }
        self.inflight.push(Inflight {
            tx,
            task: id,
            misses: 0,
            included: None,
        });
        Ok(true)
    }

    /// Per-head housekeeping for one task: arming probe, spray, retries, expiry.
    async fn tick(&mut self, ti: usize) -> Result<()> {
        let spec = self.tasks[ti].spec.clone();
        let head = self.head;
        let busy = self
            .inflight
            .iter()
            .any(|f| f.task == spec.task_id && f.included.is_none());
        match (spec.mode, self.tasks[ti].state) {
            (Mode::PreArmedSpray, St::Ready) => {
                if let Some(p) = &spec.arm_probe {
                    if let Ok(out) = rpc::eth_call(self.rpc.as_ref(), None, p.to, &p.data).await {
                        if out.len() >= 32 && U256::from_be_slice(&out[..32]) >= p.min_value {
                            self.arm(spec.task_id)?;
                        }
                    }
                }
            }
            (Mode::PreArmedSpray, St::Armed | St::Triggered) => {
                let armed = self.tasks[ti]
                    .armed_block
                    .or(self.tasks[ti].trigger_block)
                    .unwrap_or(head);
                if head > armed + spec.window_blocks || self.tasks[ti].sent >= spec.max_attempts {
                    if !busy {
                        self.set_state(
                            ti,
                            St::Expired,
                            "spray window or attempt limit reached without a sale",
                        )?;
                    }
                    return Ok(());
                }
                for _ in 0..spec.spray_per_block {
                    if self.tasks[ti].state == St::Armed {
                        if !self.store.transition(
                            spec.task_id,
                            &["armed"],
                            "triggered",
                            now_ms(),
                        )? {
                            break;
                        }
                        self.tasks[ti].state = St::Triggered;
                        self.tasks[ti].trigger_block = Some(head);
                    }
                    if !self.tasks[ti].state.open() || !self.send_attempt(ti, None, 0).await? {
                        break;
                    }
                }
            }
            (Mode::NextBlockBestEffort, St::Ready) => {
                if let Some((open, _)) = self.tasks[ti].open {
                    if head > open + spec.window_blocks {
                        self.set_state(
                            ti,
                            St::Expired,
                            "launch seen but the sale never became executable inside the window",
                        )?;
                    } else if self.tasks[ti].wait_sim {
                        self.try_fire(ti, 0).await?;
                    }
                }
            }
            (Mode::NextBlockBestEffort, St::Triggered) => {
                let start = self.tasks[ti].trigger_block.unwrap_or(head);
                let can_retry =
                    self.tasks[ti].sent < spec.max_attempts && head <= start + spec.window_blocks;
                if self.tasks[ti].wait_retry && can_retry && !busy {
                    self.tasks[ti].wait_retry = false;
                    self.send_attempt(ti, None, 0).await?;
                } else if !busy && !can_retry {
                    self.set_state(
                        ti,
                        St::Expired,
                        "attempts exhausted or window passed without a sale",
                    )?;
                }
            }
            (Mode::StrictSameBlock, St::Triggered) => {
                // Past the bundle's maxBlockNumber the task ends. It is never re-sent as a plain transaction.
                if self.tasks[ti].max_block.is_none_or(|m| head > m + 2) {
                    self.set_state(
                        ti,
                        St::Expired,
                        "bundle window passed; strict mode does not fall back",
                    )?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Receipts decide everything: status, measured amounts, cost, position, and later finality.
    async fn track(&mut self) -> Result<()> {
        if self.inflight.is_empty() {
            return Ok(());
        }
        let chain_nonce = rpc::nonce_at(self.rpc.as_ref(), self.signer.address(), "latest")
            .await
            .ok();
        let finalized = if self.inflight.iter().any(|f| f.included.is_some()) {
            rpc::head_by_tag(self.rpc.as_ref(), "finalized")
                .await
                .ok()
                .flatten()
                .map(|h| h.number)
        } else {
            None
        };
        let mut keep = vec![];
        for mut f in std::mem::take(&mut self.inflight) {
            let Some(ti) = self.tasks.iter().position(|t| t.spec.task_id == f.task) else {
                continue;
            };
            if let Some(prev) = f.included.clone() {
                if finalized.is_none_or(|n| n < prev.block) {
                    keep.push(f);
                    continue;
                }
                match rpc::receipt(self.rpc.as_ref(), f.tx.hash).await {
                    Ok(Some(r)) if r.block_hash == prev.block_hash => {
                        self.store.finish_attempt(
                            f.tx.hash,
                            if prev.success {
                                "final_ok"
                            } else {
                                "final_failed"
                            },
                            Some(prev.block),
                            Some(prev.block_hash),
                            Some(prev.tx_index),
                            Some(prev.gas_used),
                            prev.cost_wei,
                            "finalized",
                        )?;
                    }
                    Ok(_) => {
                        // Reorged out after we counted it. The signed transaction is still valid.
                        tracing::warn!(task = %f.task, hash = %f.tx.hash, "included transaction left the canonical chain");
                        self.tasks[ti].budget.reopen(
                            f.tx.nonce,
                            f.tx.hash,
                            f.tx.max_cost,
                            prev.cost_wei,
                        );
                        self.store
                            .set_spent(f.task, self.tasks[ti].budget.spent())?;
                        self.store.finish_attempt(
                            f.tx.hash, "sent", None, None, None, None, 0, "reorged",
                        )?;
                        if prev.success && self.tasks[ti].state == St::Confirmed {
                            self.tasks[ti].outcome = None;
                            self.tasks[ti].placement = None;
                            self.set_state(ti, St::Triggered, "reorg")?;
                        }
                        f.included = None;
                        keep.push(f);
                    }
                    Err(_) => keep.push(f),
                }
                continue;
            }
            match rpc::receipt(self.rpc.as_ref(), f.tx.hash).await {
                Ok(Some(r)) => {
                    let spec = &self.tasks[ti].spec;
                    let o = classify(&r, spec.task_id, spec.executor_contract, f.tx.gas_price);
                    self.tasks[ti].budget.settle(f.tx.nonce, o.cost_wei);
                    self.store
                        .set_spent(f.task, self.tasks[ti].budget.spent())?;
                    self.store.finish_attempt(
                        f.tx.hash,
                        if o.success {
                            "included_ok"
                        } else {
                            "included_failed"
                        },
                        Some(o.block),
                        Some(o.block_hash),
                        Some(o.tx_index),
                        Some(o.gas_used),
                        o.cost_wei,
                        "",
                    )?;
                    self.store
                        .metric(f.task, "gas_used", o.gas_used as i64, now_ms())?;
                    if o.success {
                        self.tasks[ti].outcome = Some(o.clone());
                        self.store.set_result(f.task, &serde_json::json!({"tx": f.tx.hash, "block": o.block, "tx_index": o.tx_index, "actual_in": o.actual_in.map(|v| v.to_string()), "received": o.received.map(|v| v.to_string()), "gas_used": o.gas_used, "cost_wei": o.cost_wei.to_string()}).to_string())?;
                        self.set_state(ti, St::Confirmed, "sale receipt with TaskExecuted")?;
                        self.record_placement(ti)?;
                    } else {
                        self.tasks[ti].wait_retry = true;
                    }
                    f.included = Some(o);
                    keep.push(f);
                }
                Ok(None) => {
                    if chain_nonce.is_some_and(|n| n > f.tx.nonce) {
                        // Nonce consumed but no receipt for this hash: give the node a few heads, then
                        // conclude another transaction used the nonce.
                        f.misses += 1;
                        if f.misses >= 3 {
                            self.tasks[ti].budget.settle(f.tx.nonce, 0);
                            self.store.finish_attempt(
                                f.tx.hash,
                                "replaced",
                                None,
                                None,
                                None,
                                None,
                                0,
                                "nonce consumed by another transaction",
                            )?;
                            self.tasks[ti].wait_retry = true;
                            continue;
                        }
                    }
                    keep.push(f);
                }
                Err(_) => keep.push(f),
            }
        }
        self.inflight = keep;
        if let Some(n) = chain_nonce {
            self.nonces.reconcile(
                n,
                self.inflight
                    .iter()
                    .filter(|f| f.included.is_none())
                    .map(|f| f.tx.nonce)
                    .max(),
            );
        }
        Ok(())
    }

    fn record_placement(&mut self, ti: usize) -> Result<()> {
        let t = &mut self.tasks[ti];
        let (Some((ob, oi)), Some(o)) = (t.open, t.outcome.as_ref()) else {
            return Ok(());
        };
        if t.placement.is_some() {
            return Ok(());
        }
        let p = placement(ob, oi, o.block, o.tx_index);
        t.placement = Some(p);
        let id = t.spec.task_id;
        self.store
            .metric(id, "block_delta", o.block as i64 - ob as i64, now_ms())?;
        self.store.metric(
            id,
            "same_block_after_launch",
            matches!(p, Placement::SameBlockAfter) as i64,
            now_ms(),
        )?;
        self.store.metric(
            id,
            "tx_index_delta",
            o.tx_index as i64 - oi as i64,
            now_ms(),
        )?;
        tracing::info!(task = %id, placement = ?p, launch_block = ob, sale_block = o.block, "placement measured from receipts");
        Ok(())
    }

    pub fn inflight_count(&self) -> usize {
        self.inflight
            .iter()
            .filter(|f| f.included.is_none())
            .count()
    }
}
