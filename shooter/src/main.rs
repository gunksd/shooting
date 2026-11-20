use alloy::primitives::{Address, B256, U256};
use alloy::sol_types::SolCall;
use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use shooter::budget::PricePolicy;
use shooter::config::{Config, Mode};
use shooter::dex_adapter::{
    self, cancelTaskCall, encode_approve, encode_create_task, noncesCall, TaskParams,
};
use shooter::engine::{Engine, EngineOpts};
use shooter::planner::{fetch_bnb_price, preflight};
use shooter::rpc::{self, ChainRpc, HttpRpc};
use shooter::signer::ExecSigner;
use shooter::store::Store;
use shooter::submitter::{Channel, JsonRpcChannel};
use shooter::types::{now_ms, ChainEvent, Head};
use shooter::watcher::{backfill, run_ws, Merger, WsSource};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[derive(Parser)]
#[command(version, about = "BSC launch-sell executor")]
struct Cli {
    #[arg(long, default_value = "config.toml", global = true)]
    config: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Watch and execute. Honors `dry_run` in the config (default true).
    Run,
    /// Read chain state and report every precondition of every configured task.
    Preflight,
    /// Print tasks, attempts and measured metrics from the database.
    Status,
    /// Stop a task locally (does not touch the chain; prints the on-chain cancel calldata).
    Cancel { task: B256 },
    /// Record an initialization transaction's cost (deploy / approve / createTask).
    InitCost { task: B256, label: String, tx: B256 },
    /// Print the executor account address as JSON.
    Address,
    /// Send the executor account's whole BNB balance (minus the transfer fee) to an address.
    Withdraw { to: Address },
    /// Print the 4-byte selector of a function signature such as totalSold().
    Sig { signature: String },
    /// Deploy SellExecutor from the executor account. Broadcasts a real transaction.
    Deploy {
        /// forge artifact, e.g. contracts/out/SellExecutor.sol/SellExecutor.json
        #[arg(long)]
        artifact: PathBuf,
    },
    /// Build approve + createTask calldata for the owner wallet. Sends nothing.
    PlanTask {
        #[arg(long)]
        executor_contract: Address,
        #[arg(long)]
        owner: Address,
        #[arg(long)]
        token: Address,
        #[arg(long)]
        token_out: Address,
        #[arg(long)]
        router: Address,
        #[arg(long)]
        factory: Address,
        #[arg(long)]
        recipient: Address,
        /// Raw token units.
        #[arg(long)]
        amount: U256,
        /// Minimum the recipient must receive for the full amount, raw units of token_out.
        #[arg(long)]
        min_out_floor: U256,
        #[arg(long, default_value_t = 7 * 86400)]
        valid_secs: u64,
        /// Sell through the router (two token transfers) instead of sending straight to the pair.
        #[arg(long)]
        router_hop: bool,
        #[arg(long)]
        native_out: bool,
        #[arg(long, default_value_t = 0)]
        max_transfer_loss_bps: u16,
        #[arg(long, default_value_t = 25)]
        pair_fee_bps: u16,
    },
}

fn signer(cfg: &Config) -> Result<ExecSigner> {
    match (&cfg.executor.key_file, &cfg.executor.key_env) {
        (Some(f), _) => ExecSigner::from_file(f, cfg.chain_id),
        (_, Some(v)) => ExecSigner::from_env(v, cfg.chain_id),
        _ => bail!("no executor key source configured"),
    }
}

fn policy(cfg: &Config) -> PricePolicy {
    PricePolicy {
        max_age_s: cfg.budget.price_max_age_s,
        min_price_micros: cfg.budget.min_price_micros,
        max_price_micros: cfg.budget.max_price_micros,
        buffer_bps: cfg.budget.buffer_bps,
    }
}

fn now_s() -> u64 {
    now_ms() / 1000
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let cli = Cli::parse();
    let cfg = Config::load(&cli.config)?;
    // The trigger path uses the tight configured timeout. Setup and admin commands are not latency
    // sensitive and must tolerate a slow node (gas estimation, cold state), so they get a patient client.
    let mk =
        |t: Duration| HttpRpc::new(&cfg.rpc.http[0], None, t).map_err(|e| anyhow::anyhow!("{e}"));
    let http: Arc<dyn ChainRpc> = Arc::new(mk(Duration::from_secs(30))?);
    let fast: Arc<dyn ChainRpc> = Arc::new(mk(Duration::from_millis(cfg.rpc.timeout_ms))?);

    match cli.cmd {
        Cmd::Run => run(cfg, fast, http).await,
        Cmd::Preflight => {
            let s = signer(&cfg)?;
            let q = fetch_bnb_price(
                http.as_ref(),
                cfg.budget.price_pair,
                cfg.budget.price_stable,
                now_s(),
            )
            .await?;
            let cap =
                shooter::budget::cap_from_usd(cfg.budget.usd_micros, q, now_s(), &policy(&cfg))?;
            println!(
                "executor account {}  BNB price {} micro-USDT  budget cap {} wei",
                s.address(),
                q.price_micros,
                cap
            );
            let mut all_ok = true;
            for t in &cfg.tasks {
                println!("task {}", t.task_id);
                for c in preflight(http.as_ref(), t, s.address(), cap).await? {
                    all_ok &= c.ok;
                    println!(
                        "  [{}] {:<22} {}",
                        if c.ok { "ok" } else { "FAIL" },
                        c.name,
                        c.detail
                    );
                }
            }
            if !all_ok {
                bail!("preflight failed");
            }
            Ok(())
        }
        Cmd::Address => {
            println!(
                "{}",
                serde_json::json!({"address": signer(&cfg)?.address()})
            );
            Ok(())
        }
        Cmd::Withdraw { to } => {
            let s = signer(&cfg)?;
            let price = rpc::gas_price(http.as_ref()).await?;
            // 21000 for a plain account; estimate in case the destination is a contract wallet.
            let est = http
                .request(
                    "eth_estimateGas",
                    serde_json::json!([{"from": s.address(), "to": to, "value": "0x1"}]),
                )
                .await
                .ok();
            let gas = est
                .as_ref()
                .and_then(|v| v.as_str())
                .and_then(shooter::types::hex_u64)
                .unwrap_or(21_000)
                .max(21_000);
            let fee = U256::from(gas as u128 * price);
            let bal = rpc::balance(http.as_ref(), s.address()).await?;
            if bal <= fee {
                bail!("balance does not cover the transfer fee");
            }
            let nonce = rpc::nonce_at(http.as_ref(), s.address(), "pending").await?;
            let tx = s.sign(nonce, price, gas, to, bal - fee, Default::default())?;
            rpc::send_raw(http.as_ref(), &tx.raw).await?;
            let mut confirmed = false;
            for _ in 0..60 {
                if let Some(r) = rpc::receipt(http.as_ref(), tx.hash).await? {
                    if r.status != Some(1) {
                        bail!("withdraw transaction {} reverted; the balance (minus gas) is still in the executor account", tx.hash);
                    }
                    confirmed = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            println!(
                "{}",
                serde_json::json!({"tx": tx.hash, "to": to, "amount_wei": (bal - fee).to_string(), "confirmed": confirmed})
            );
            Ok(())
        }
        Cmd::Sig { signature } => {
            let h = alloy::primitives::keccak256(signature.as_bytes());
            println!(
                "{}",
                serde_json::json!({"selector": format!("0x{}", alloy::primitives::hex::encode(&h[..4]))})
            );
            Ok(())
        }
        Cmd::Deploy { artifact } => {
            let s = signer(&cfg)?;
            let v: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&artifact).context("artifact")?)?;
            let code: alloy::primitives::Bytes = alloy::primitives::hex::decode(
                v["bytecode"]["object"]
                    .as_str()
                    .context("artifact has no bytecode")?,
            )?
            .into();
            let est = http
                .request(
                    "eth_estimateGas",
                    serde_json::json!([{"from": s.address(), "data": code}]),
                )
                .await?;
            let gas =
                shooter::types::hex_u64(est.as_str().unwrap_or("")).context("estimate")? * 12 / 10;
            let price = rpc::gas_price(http.as_ref()).await?;
            let nonce = rpc::nonce_at(http.as_ref(), s.address(), "pending").await?;
            let tx = s.sign_create(nonce, price, gas, code)?;
            rpc::send_raw(http.as_ref(), &tx.raw).await?;
            for _ in 0..120 {
                if let Some(r) = rpc::receipt(http.as_ref(), tx.hash).await? {
                    if r.status != Some(1) {
                        bail!("deployment transaction {} reverted", tx.hash);
                    }
                    println!(
                        "{}",
                        serde_json::json!({"address": s.address().create(nonce), "tx": tx.hash, "cost_wei": (r.gas_used as u128 * price).to_string()})
                    );
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            bail!(
                "deployment transaction {} not confirmed within 60s; check it before retrying",
                tx.hash
            )
        }
        Cmd::Status => {
            let store = Store::open(&cfg.db_path)?;
            let mut tasks = vec![];
            for t in &cfg.tasks {
                let row = store.task(t.task_id)?;
                tasks.push(serde_json::json!({
                    "task_id": t.task_id, "mode": t.mode,
                    "state": row.as_ref().map(|r| r.state.clone()).unwrap_or_else(|| "not_started".into()),
                    "attempts": row.as_ref().map(|r| r.attempts).unwrap_or(0),
                    "cap_wei": row.as_ref().map(|r| r.cap_wei.to_string()), "spent_wei": row.as_ref().map(|r| r.spent_wei.to_string()),
                    "launch_block": row.as_ref().and_then(|r| r.open_block),
                    "init_costs_wei": store.init_cost_total(t.task_id)?.to_string(),
                    "metrics": store.metrics(t.task_id)?, "result": store.task_result(t.task_id)?,
                }));
            }
            let attempts: Vec<_> = store.attempts_in_state(&["signed", "sent", "unknown", "included_ok", "included_failed", "final_ok", "final_failed", "replaced", "rejected", "not_sent", "dry_run"])?
                .into_iter().map(|a| serde_json::json!({"tx": a.tx_hash, "task_id": a.task_id, "nonce": a.nonce, "state": a.state, "max_cost_wei": a.max_cost.to_string(), "block": a.submitted_block})).collect();
            println!(
                "{}",
                serde_json::json!({"tasks": tasks, "attempts": attempts})
            );
            Ok(())
        }
        Cmd::Cancel { task } => {
            let store = Store::open(&cfg.db_path)?;
            let done = store.transition(
                task,
                &["ready", "armed", "triggered"],
                "cancelled",
                now_ms(),
            )?;
            println!(
                "local task {}",
                if done { "cancelled" } else { "was not open" }
            );
            println!("transactions already sent cannot be recalled; to revoke on chain, have the owner wallet send:");
            println!(
                "  cancelTask calldata: 0x{}",
                alloy::primitives::hex::encode(cancelTaskCall { taskId: task }.abi_encode())
            );
            println!("  and/or approve(executor_contract, 0) on the token");
            Ok(())
        }
        Cmd::InitCost { task, label, tx } => {
            let r = rpc::receipt(http.as_ref(), tx)
                .await?
                .context("receipt not found")?;
            let price: u128 = r
                .effective_gas_price
                .context("no effectiveGasPrice")?
                .try_into()
                .map_err(|_| anyhow::anyhow!("price overflow"))?;
            let cost = r.gas_used as u128 * price;
            Store::open(&cfg.db_path)?.add_init_cost(task, &label, cost, &tx.to_string())?;
            println!("recorded {label}: {cost} wei");
            Ok(())
        }
        Cmd::PlanTask {
            executor_contract,
            owner,
            token,
            token_out,
            router,
            factory,
            recipient,
            amount,
            min_out_floor,
            valid_secs,
            router_hop,
            native_out,
            max_transfer_loss_bps,
            pair_fee_bps,
        } => {
            let exec = signer(&cfg)?.address();
            let nonce = U256::from_be_slice(
                &rpc::eth_call(
                    http.as_ref(),
                    None,
                    executor_contract,
                    &noncesCall { owner }.abi_encode().into(),
                )
                .await?,
            );
            let p = TaskParams {
                executor: exec,
                tokenIn: token,
                tokenOut: token_out,
                router,
                factory,
                recipient,
                maxAmountIn: amount,
                minOutFloor: min_out_floor,
                deadline: now_s() + valid_secs,
                minBlock: 0,
                maxBlock: 0,
                maxTransferLossBps: max_transfer_loss_bps,
                pairFeeBps: if router_hop { 0 } else { pair_fee_bps },
                mode: if router_hop { 0 } else { 1 },
                nativeOut: native_out,
                path: vec![token, token_out],
            };
            println!(
                "{}",
                serde_json::json!({
                    "task_id": dex_adapter::task_id(cfg.chain_id, executor_contract, owner, nonce), "executor": exec,
                    "approve": {"to": token, "data": encode_approve(executor_contract, amount)},
                    "create_task": {"to": executor_contract, "data": encode_create_task(&p)},
                })
            );
            Ok(())
        }
    }
}

/// `http` is the tight-timeout client used on the trigger path; `patient` is for startup reads.
async fn run(cfg: Config, http: Arc<dyn ChainRpc>, patient: Arc<dyn ChainRpc>) -> Result<()> {
    let remote = rpc::chain_id(patient.as_ref()).await?;
    if remote != cfg.chain_id {
        bail!(
            "RPC chain id {remote} does not match configured {}",
            cfg.chain_id
        );
    }
    let timeout = Duration::from_millis(cfg.rpc.timeout_ms);
    let signer = Arc::new(signer(&cfg)?);
    let store = Arc::new(Store::open(&cfg.db_path)?);
    let mut channels: Vec<Arc<dyn Channel>> = vec![];
    for c in &cfg.channels {
        channels.push(Arc::new(JsonRpcChannel::from_config(c, timeout)?));
    }
    let opts = EngineOpts {
        dry_run: cfg.dry_run,
        submit_timeout: timeout,
        has_pending_source: !cfg.rpc.pending_ws.is_empty(),
        usd_micros: cfg.budget.usd_micros,
        price_policy: policy(&cfg),
        include_init_costs: cfg.budget.include_init_costs,
    };
    let mut engine = Engine::new(opts, http.clone(), store, signer.clone(), channels).await?;
    tracing::info!(executor = %signer.address(), dry_run = cfg.dry_run, "starting");

    let quote = fetch_bnb_price(
        patient.as_ref(),
        cfg.budget.price_pair,
        cfg.budget.price_stable,
        now_s(),
    )
    .await
    .ok();
    for t in &cfg.tasks {
        if let Err(e) = engine.add_task(t.clone(), quote, now_s()) {
            tracing::error!(task = %t.task_id, error = %e, "task refused");
        }
    }
    engine.recover().await?;
    for t in &cfg.tasks {
        if t.mode == Mode::PreArmedSpray && t.arm_probe.is_none() {
            engine.arm(t.task_id)?;
        }
    }
    if engine.tasks.is_empty() {
        bail!("no runnable task");
    }

    let addresses = engine.addresses();
    let last_block = Arc::new(AtomicU64::new(engine.head));
    let (tx, mut rx) = tokio::sync::mpsc::channel::<ChainEvent>(4096);
    let rules: Vec<_> = cfg
        .tasks
        .iter()
        .flat_map(|t| t.strict_targets.clone())
        .collect();
    let mut index = 0;
    for (urls, pending) in [(&cfg.rpc.ws, false), (&cfg.rpc.pending_ws, true)] {
        for url in urls {
            let src = WsSource {
                url: url.clone(),
                index,
                addresses: addresses.clone(),
                pending_rules: if pending { rules.clone() } else { vec![] },
                backfill_blocks: cfg.rpc.backfill_blocks,
            };
            tokio::spawn(run_ws(src, http.clone(), last_block.clone(), tx.clone()));
            index += 1;
        }
    }
    // HTTP poller: the only source when no WebSocket is configured, and a safety net otherwise.
    {
        let (http, tx, addresses, last_block) = (
            http.clone(),
            tx.clone(),
            addresses.clone(),
            last_block.clone(),
        );
        let interval = Duration::from_millis(if cfg.rpc.ws.is_empty() { 120 } else { 1000 });
        tokio::spawn(async move {
            let mut seen = last_block.load(Ordering::Relaxed);
            loop {
                tokio::time::sleep(interval).await;
                let Ok(tip) = rpc::block_number(http.as_ref()).await else {
                    continue;
                };
                if tip <= seen {
                    continue;
                }
                if let Ok(evs) =
                    backfill(http.as_ref(), &addresses, seen + 1, tip, usize::MAX).await
                {
                    for ev in evs {
                        let _ = tx.send(ev).await;
                    }
                    if let Ok(Some(head)) =
                        rpc::head_by_tag(http.as_ref(), &format!("0x{tip:x}")).await
                    {
                        let _ = tx
                            .send(ChainEvent::Head {
                                head: Head { ..head },
                                source: usize::MAX,
                                seen_ms: now_ms(),
                            })
                            .await;
                    }
                    seen = tip;
                    last_block.fetch_max(tip, Ordering::Relaxed);
                }
            }
        });
    }
    // Logs between the last run and now.
    let from = engine.head.saturating_sub(cfg.rpc.backfill_blocks);
    for ev in backfill(http.as_ref(), &addresses, from, engine.head, usize::MAX)
        .await
        .unwrap_or_default()
    {
        let _ = tx.send(ev).await;
    }

    let mut merger = Merger::new(65_536);
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                tracing::info!(open_transactions = engine.inflight_count(), "stopping; open transactions stay recorded and are picked up on the next start");
                return Ok(());
            }
            ev = rx.recv() => {
                let Some(first) = ev else { return Ok(()) };
                // Drain what queued up while the engine was busy. Logs and pending transactions are all
                // handled in order; of the heads only the newest one, so the engine never acts on a stale tip.
                let mut batch = vec![first];
                while let Ok(more) = rx.try_recv() {
                    batch.push(more);
                }
                let mut newest_head = None;
                for ev in batch.into_iter().filter_map(|ev| merger.accept(ev)) {
                    if matches!(ev, ChainEvent::Head { .. }) {
                        newest_head = Some(ev);
                    } else if let Err(e) = engine.on_event(ev).await {
                        tracing::error!(error = %e, "event handling failed");
                    }
                }
                if let Some(head) = newest_head {
                    if let Err(e) = engine.on_event(head).await {
                        tracing::error!(error = %e, "head handling failed");
                    }
                }
            }
        }
    }
}
