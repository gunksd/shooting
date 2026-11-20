//! End-to-end against a real EVM (anvil) with the real SellExecutor bytecode, a working V2 AMM and a
//! token whose transfers are blocked until launch. Nothing is mocked: the engine talks JSON-RPC,
//! signs real transactions and reads real receipts.
//! Needs `anvil` on PATH and `forge build` output in ../contracts/out; skipped (with a note) otherwise.
use alloy::primitives::{address, Address, Bytes, B256, U256};
use alloy::sol;
use alloy::sol_types::{SolCall, SolValue};
use serde_json::{json, Value};
use shooter::budget::{PricePolicy, PriceQuote};
use shooter::config::{ChannelKind, Mode, TaskSpec};
use shooter::dex_adapter::{
    balanceOfCall, encode_approve, encode_create_task, task_id, TaskParams,
};
use shooter::engine::{Engine, EngineOpts, St};
use shooter::planner::preflight;
use shooter::receipt_tracker::Placement;
use shooter::rpc::{self, ChainRpc, HttpRpc};
use shooter::signer::ExecSigner;
use shooter::store::Store;
use shooter::submitter::{Channel, JsonRpcChannel};
use shooter::types::{now_ms, ChainEvent};
use shooter::watcher::{backfill, Merger};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

sol! {
    function issue(address to, uint256 amount);
    function launch() payable;
    function token() returns (address);
    function INIT_CODE_HASH() returns (bytes32);
}

const DEPLOYER: Address = address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266"); // anvil #0
const KEEPER_KEY: &str = "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d"; // anvil #1
const OWNER: Address = address!("3C44CdDdB6a900fa2b585dd299e03d12FA4293BC"); // anvil #2
const RECIPIENT: Address = address!("00000000000000000000000000000000000000aa");
const GWEI: u128 = 1_000_000_000;

struct Anvil(Child);
impl Drop for Anvil {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn bytecode(file: &str, name: &str) -> Option<Vec<u8>> {
    let p = format!(
        "{}/../contracts/out/{file}/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let v: Value = serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()?;
    alloy::primitives::hex::decode(v["bytecode"]["object"].as_str()?).ok()
}

struct Env {
    _anvil: Anvil,
    rpc: Arc<HttpRpc>,
    executor: Address,
    factory: Address,
    weth: Address,
    launcher: Address,
    token: Address,
    init_hash: B256,
}

impl Env {
    async fn send(
        &self,
        from: Address,
        to: Option<Address>,
        data: Vec<u8>,
        value: u128,
        gas_price: u128,
    ) -> B256 {
        let mut tx = json!({"from": from, "data": Bytes::from(data), "value": format!("0x{value:x}"), "gas": "0x7a1200", "gasPrice": format!("0x{gas_price:x}")});
        if let Some(t) = to {
            tx["to"] = json!(t);
        }
        serde_json::from_value(
            self.rpc
                .request("eth_sendTransaction", json!([tx]))
                .await
                .expect("send"),
        )
        .unwrap()
    }
    /// Polls until the transaction is mined (automine is asynchronous).
    async fn wait(&self, h: B256) -> Value {
        for _ in 0..200 {
            let r = self
                .rpc
                .request("eth_getTransactionReceipt", json!([h]))
                .await
                .unwrap();
            if !r.is_null() {
                return r;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("transaction {h} not mined");
    }
    async fn deploy(&self, code: Vec<u8>) -> Address {
        let h = self.send(DEPLOYER, None, code, 0, GWEI).await;
        let r = self.wait(h).await;
        assert_eq!(r["status"], "0x1", "deployment failed");
        serde_json::from_value(r["contractAddress"].clone()).unwrap()
    }
    async fn call(&self, to: Address, data: Vec<u8>) -> Bytes {
        rpc::eth_call(self.rpc.as_ref(), None, to, &data.into())
            .await
            .unwrap()
    }
    async fn weth_of(&self, who: Address) -> U256 {
        U256::from_be_slice(
            &self
                .call(self.weth, balanceOfCall { who }.abi_encode())
                .await,
        )
    }
    async fn mine(&self) {
        self.rpc.request("evm_mine", json!([])).await.unwrap();
    }
}

async fn setup(port: u16) -> Option<Env> {
    let (Some(ex), Some(fac), Some(weth), Some(launcher)) = (
        bytecode("SellExecutor.sol", "SellExecutor"),
        bytecode("MiniV2.sol", "MiniFactory"),
        bytecode("MiniV2.sol", "MiniWETH"),
        bytecode("Launcher.sol", "Launcher"),
    ) else {
        eprintln!("SKIPPED: run `forge build` in contracts/ first");
        return None;
    };
    let child = Command::new("anvil")
        .args([
            "--port",
            &port.to_string(),
            "--chain-id",
            "56",
            "--block-base-fee-per-gas",
            "0",
            "--gas-price",
            "0",
            "--silent",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let Ok(child) = child else {
        eprintln!("SKIPPED: anvil not found on PATH");
        return None;
    };
    let anvil = Anvil(child);
    let rpc = Arc::new(
        HttpRpc::new(
            &format!("http://127.0.0.1:{port}"),
            None,
            Duration::from_secs(5),
        )
        .unwrap(),
    );
    for _ in 0..100 {
        if rpc::block_number(rpc.as_ref()).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let mut e = Env {
        _anvil: anvil,
        rpc,
        executor: Address::ZERO,
        factory: Address::ZERO,
        weth: Address::ZERO,
        launcher: Address::ZERO,
        token: Address::ZERO,
        init_hash: B256::ZERO,
    };
    e.executor = e.deploy(ex).await;
    e.factory = e.deploy(fac).await;
    e.weth = e.deploy(weth).await;
    e.launcher = e
        .deploy([launcher, (e.factory, e.weth).abi_encode()].concat())
        .await;
    e.token = Address::from_slice(&e.call(e.launcher, tokenCall {}.abi_encode()).await[12..]);
    e.init_hash = B256::from_slice(&e.call(e.factory, INIT_CODE_HASHCall {}.abi_encode()).await);
    Some(e)
}

/// Owner-side preparation exactly as it happens for real: tokens issued before launch (not
/// transferable), then approve + createTask while trading is still closed.
async fn prepare_task(e: &Env, keeper: Address, amount: U256, floor: U256) -> B256 {
    let h = e
        .send(
            DEPLOYER,
            Some(e.launcher),
            issueCall { to: OWNER, amount }.abi_encode(),
            0,
            GWEI,
        )
        .await;
    e.wait(h).await;
    let h = e
        .send(
            OWNER,
            Some(e.token),
            encode_approve(e.executor, amount).to_vec(),
            0,
            GWEI,
        )
        .await;
    assert_eq!(
        e.wait(h).await["status"],
        "0x1",
        "approve before launch must work"
    );
    // The router is only pinned for validation in DirectToPair mode; any contract whose factory() matches works.
    let router = e
        .deploy(
            [
                bytecode("MiniV2.sol", "MiniRouter").unwrap(),
                (e.factory, e.weth).abi_encode(),
            ]
            .concat(),
        )
        .await;
    let now = u64::from_str_radix(
        e.rpc
            .request("eth_getBlockByNumber", json!(["latest", false]))
            .await
            .unwrap()["timestamp"]
            .as_str()
            .unwrap()
            .trim_start_matches("0x"),
        16,
    )
    .unwrap();
    let p = TaskParams {
        executor: keeper,
        tokenIn: e.token,
        tokenOut: e.weth,
        router,
        factory: e.factory,
        recipient: RECIPIENT,
        maxAmountIn: amount,
        minOutFloor: floor,
        deadline: now + 86_400,
        minBlock: 0,
        maxBlock: 0,
        maxTransferLossBps: 0,
        pairFeeBps: 25,
        mode: 1,
        nativeOut: false,
        path: vec![e.token, e.weth],
    };
    let h = e
        .send(
            OWNER,
            Some(e.executor),
            encode_create_task(&p).to_vec(),
            0,
            GWEI,
        )
        .await;
    assert_eq!(
        e.wait(h).await["status"],
        "0x1",
        "createTask before launch must work"
    );
    task_id(56, e.executor, OWNER, U256::ZERO)
}

fn spec(e: &Env, id: B256, mode: Mode, amount: U256, min_out: U256) -> TaskSpec {
    TaskSpec {
        task_id: id,
        executor_contract: e.executor,
        owner: OWNER,
        token_in: e.token,
        token_out: e.weth,
        factory: e.factory,
        pair_init_code_hash: e.init_hash,
        pair_fee_bps: 25,
        amount_in: amount,
        min_out,
        mode,
        gas_limit: 400_000,
        gas_price_wei: GWEI,
        channels: vec!["anvil".into()],
        allow_public_with_private: false,
        max_attempts: 6,
        window_blocks: 30,
        require_sim_ok: true,
        spray_per_block: 1,
        arm_probe: None,
        strict_targets: vec![],
    }
}

async fn engine(e: &Env, store: &Arc<Store>) -> (Engine, Address) {
    let signer = Arc::new(ExecSigner::from_secret(KEEPER_KEY, 56).unwrap());
    let keeper = signer.address();
    let rpc: Arc<dyn ChainRpc> = e.rpc.clone();
    let ch: Vec<Arc<dyn Channel>> = vec![Arc::new(JsonRpcChannel::new(
        "anvil",
        ChannelKind::Public,
        rpc.clone(),
        Duration::ZERO,
    ))];
    let opts = EngineOpts {
        dry_run: false,
        submit_timeout: Duration::from_secs(3),
        has_pending_source: false,
        usd_micros: 500_000,
        price_policy: PricePolicy {
            max_age_s: 30,
            min_price_micros: 1,
            max_price_micros: u64::MAX,
            buffer_bps: 500,
        },
        include_init_costs: false,
    };
    (
        Engine::new(opts, rpc, store.clone(), signer, ch)
            .await
            .unwrap(),
        keeper,
    )
}

/// Feeds the engine what a live watcher would deliver for blocks (from, tip]: logs, then the head.
async fn pump(e: &Env, eng: &mut Engine, merger: &mut Merger, from: u64) -> u64 {
    let tip = rpc::block_number(e.rpc.as_ref()).await.unwrap();
    for b in from + 1..=tip {
        for ev in backfill(e.rpc.as_ref(), &eng.addresses(), b, b, 0)
            .await
            .unwrap()
        {
            if let Some(ev) = merger.accept(ev) {
                eng.on_event(ev).await.unwrap();
            }
        }
        let head = rpc::head_by_tag(e.rpc.as_ref(), &format!("0x{b:x}"))
            .await
            .unwrap()
            .unwrap();
        eng.on_event(
            merger
                .accept(ChainEvent::Head {
                    head,
                    source: 0,
                    seen_ms: now_ms(),
                })
                .unwrap(),
        )
        .await
        .unwrap();
    }
    tip
}

fn quote() -> Option<PriceQuote> {
    // 0.5 USDT at 0.0005 USDT/BNB = 1000 BNB cap: anvil gas prices are arbitrary, the cap logic is covered elsewhere.
    Some(PriceQuote {
        price_micros: 500,
        fetched_at_s: now_ms() / 1000,
    })
}

#[tokio::test]
async fn next_block_sale_against_real_contracts() {
    let Some(e) = setup(18545).await else { return };
    let store = Arc::new(Store::memory().unwrap());
    let amount = U256::from(500_000u64) * U256::from(10u64).pow(U256::from(18u64));
    let floor = U256::from(30_000_000_000_000_000u64); // 0.03 BNB = 2x of a 0.015 cost
    let (mut eng, keeper) = engine(&e, &store).await;
    let id = prepare_task(&e, keeper, amount, floor).await;
    let s = spec(&e, id, Mode::NextBlockBestEffort, amount, floor);

    let checks = preflight(e.rpc.as_ref(), &s, keeper, 10u128.pow(18))
        .await
        .unwrap();
    assert!(checks.iter().all(|c| c.ok), "{checks:?}");
    eng.add_task(s, quote(), now_ms() / 1000).unwrap();

    let mut merger = Merger::new(4096);
    let start = eng.head;
    let mut seen = pump(&e, &mut eng, &mut merger, start).await;
    assert_eq!(
        eng.task(id).unwrap().state,
        St::Ready,
        "nothing happens before the launch"
    );

    let h = e
        .send(
            DEPLOYER,
            Some(e.launcher),
            launchCall {}.abi_encode(),
            18 * 10u128.pow(18),
            GWEI,
        )
        .await;
    assert_eq!(e.wait(h).await["status"], "0x1");
    let launch_block = rpc::block_number(e.rpc.as_ref()).await.unwrap();
    seen = pump(&e, &mut eng, &mut merger, seen).await; // sees the launch, sends; anvil automines it
                                                        // anvil automines the sale; it may already be confirmed within the same pump.
    let sale = store.attempts_in_state(&["sent", "included_ok"]).unwrap();
    assert_eq!(sale.len(), 1);
    e.wait(sale[0].tx_hash).await;
    pump(&e, &mut eng, &mut merger, seen).await;

    let t = eng.task(id).unwrap();
    assert_eq!(t.state, St::Confirmed, "{}", t.reason);
    assert_eq!(t.placement, Some(Placement::BlocksAfter(1)));
    let o = t.outcome.clone().unwrap();
    assert_eq!(o.block, launch_block + 1);
    let got = e.weth_of(RECIPIENT).await;
    assert_eq!(
        Some(got),
        o.received,
        "event amount equals the real balance change"
    );
    assert_eq!(
        got,
        U256::from(44_775_840_248_380_600u64),
        "same pool maths as the mainnet SLERF trade"
    );
    assert_eq!(o.actual_in, Some(amount));
    assert_eq!(t.budget.spent(), o.gas_used as u128 * GWEI);
    eprintln!("measured on anvil: sale gas_used = {}", o.gas_used);
}

#[tokio::test]
async fn pre_armed_sale_lands_in_the_launch_block() {
    let Some(e) = setup(18546).await else { return };
    let store = Arc::new(Store::memory().unwrap());
    let amount = U256::from(500_000u64) * U256::from(10u64).pow(U256::from(18u64));
    let floor = U256::from(30_000_000_000_000_000u64);
    let (mut eng, keeper) = engine(&e, &store).await;
    let id = prepare_task(&e, keeper, amount, floor).await;
    let mut s = spec(&e, id, Mode::PreArmedSpray, amount, floor);
    s.require_sim_ok = false;
    eng.add_task(s, quote(), now_ms() / 1000).unwrap();
    eng.arm(id).unwrap();
    e.rpc
        .request("evm_setAutomine", json!([false]))
        .await
        .unwrap();

    let mut merger = Merger::new(4096);
    let start = rpc::block_number(e.rpc.as_ref()).await.unwrap();
    e.mine().await;
    let mut seen = pump(&e, &mut eng, &mut merger, start).await; // attempt 1 enters the mempool
    e.mine().await; // attempt 1 mined before any pool exists: cheap revert
    seen = pump(&e, &mut eng, &mut merger, seen).await; // attempt 2 enters the mempool
    let failed = store.attempts_in_state(&["included_failed"]).unwrap();
    assert_eq!(failed.len(), 1);

    // The launch pays a higher gas price, so the block orders it ahead of our waiting attempt.
    e.send(
        DEPLOYER,
        Some(e.launcher),
        launchCall {}.abi_encode(),
        18 * 10u128.pow(18),
        2 * GWEI,
    )
    .await;
    e.mine().await;
    seen = pump(&e, &mut eng, &mut merger, seen).await;
    e.mine().await;
    pump(&e, &mut eng, &mut merger, seen).await;

    let t = eng.task(id).unwrap();
    assert_eq!(t.state, St::Confirmed, "{}", t.reason);
    assert_eq!(
        t.placement,
        Some(Placement::SameBlockAfter),
        "sold in the launch block, after the launch"
    );
    assert_eq!(
        e.weth_of(RECIPIENT).await,
        U256::from(44_775_840_248_380_600u64)
    );
    let pre_gas = store
        .metrics(id)
        .unwrap()
        .iter()
        .find(|m| m.0 == "gas_used")
        .unwrap()
        .1;
    eprintln!(
        "measured on anvil: pre-launch failed attempt gas_used = {pre_gas}, sale gas_used = {}",
        t.outcome.as_ref().unwrap().gas_used
    );
    assert!(pre_gas < 80_000);
    assert_eq!(
        store.attempts_in_state(&["included_ok"]).unwrap().len()
            + store.attempts_in_state(&["final_ok"]).unwrap().len(),
        1,
        "sold exactly once"
    );
}
