mod common;
use alloy::primitives::{fixed_bytes, Bytes, B256};
use common::*;
use shooter::config::{ChannelKind, Mode, PendingRule, Probe};
use shooter::engine::St;
use shooter::receipt_tracker::Placement;
use shooter::store::Store;
use shooter::types::{ChainEvent, Head, PendingTx};
use shooter::watcher::{backfill, Merger};
use std::sync::Arc;

const PRICE: u128 = 50_000_000;
const MAX_COST: u128 = 300_000 * PRICE;

fn head(n: u64) -> ChainEvent {
    ChainEvent::Head {
        head: Head {
            number: n,
            hash: B256::repeat_byte(n as u8),
            parent_hash: B256::ZERO,
            timestamp: n,
        },
        source: 0,
        seen_ms: 0,
    }
}
fn store() -> Arc<Store> {
    Arc::new(Store::memory().unwrap())
}

#[tokio::test]
async fn duplicate_events_fire_once_and_next_block_is_measured() {
    let (sim, st) = (SimChain::new(), store());
    let mut e = engine(&sim, &st, opts(), ChannelKind::Public).await;
    e.add_task(spec(Mode::NextBlockBestEffort), quote(), NOW_S)
        .unwrap();

    sim.set(|i| i.opened = true);
    let mut merger = Merger::new(1024);
    for source in 0..3 {
        // three upstreams deliver the same launch log
        if let Some(ev) = merger.accept(log_event(liquidity_log(101, 48), source)) {
            e.on_event(ev).await.unwrap();
        }
        // and even if the merger were bypassed, the adapter and the DB guard hold
        e.on_event(log_event(liquidity_log(101, 48), source))
            .await
            .unwrap();
    }
    assert_eq!(
        sim.get(|i| i.accepted.len()),
        1,
        "exactly one transaction left the process"
    );

    sim.set(|i| i.block = 101);
    let h = sim.mine(None, 3);
    e.on_event(h).await.unwrap();
    let t = e.task(TASK).unwrap();
    assert_eq!(t.state, St::Confirmed);
    assert_eq!(t.placement, Some(Placement::BlocksAfter(1)));
    assert_eq!(t.budget.spent(), GAS_OK as u128 * PRICE);
    assert_eq!(t.budget.reserved(), 0);
    let m = st.metrics(TASK).unwrap();
    assert!(m.contains(&("block_delta".to_string(), 1)));
    assert!(m.contains(&("same_block_after_launch".to_string(), 0)));
    assert!(m.iter().any(|(n, _)| n == "submit_ms:main") && m.iter().any(|(n, _)| n == "sign_us"));
}

#[tokio::test]
async fn competing_nodes_trigger_once() {
    // Two engine processes share one database and both see the launch.
    let (sim, st) = (SimChain::new(), store());
    let mut a = engine(&sim, &st, opts(), ChannelKind::Public).await;
    let mut b = engine(&sim, &st, opts(), ChannelKind::Public).await;
    a.add_task(spec(Mode::NextBlockBestEffort), quote(), NOW_S)
        .unwrap();
    b.add_task(spec(Mode::NextBlockBestEffort), None, NOW_S)
        .unwrap();
    let (ra, rb) = tokio::join!(
        a.on_event(log_event(liquidity_log(101, 48), 0)),
        b.on_event(log_event(liquidity_log(101, 48), 1))
    );
    ra.unwrap();
    rb.unwrap();
    assert_eq!(sim.get(|i| i.accepted.len()), 1);
    assert_eq!(st.task(TASK).unwrap().unwrap().attempts, 1);
}

#[tokio::test]
async fn missed_log_is_recovered_by_backfill_exactly_once() {
    let (sim, st) = (SimChain::new(), store());
    let mut e = engine(&sim, &st, opts(), ChannelKind::Public).await;
    e.add_task(spec(Mode::NextBlockBestEffort), quote(), NOW_S)
        .unwrap();
    let launch_head = sim.mine(Some(48), 0); // block 101 holds the launch; the live stream missed its log
    let mut merger = Merger::new(1024);
    let events = backfill(sim.as_ref(), &e.addresses(), 90, 101, 0)
        .await
        .unwrap();
    assert_eq!(events.len(), 1);
    for ev in events {
        e.on_event(merger.accept(ev).unwrap()).await.unwrap();
    }
    // the upstream replays the same log after reconnecting
    assert!(merger
        .accept(log_event(liquidity_log(101, 48), 1))
        .is_none());
    e.on_event(launch_head).await.unwrap();
    assert_eq!(sim.get(|i| i.accepted.len()), 1);
    e.on_event(sim.mine(None, 0)).await.unwrap();
    assert_eq!(e.task(TASK).unwrap().state, St::Confirmed);
}

#[tokio::test]
async fn removed_launch_log_cancels_the_pending_trigger() {
    let (sim, st) = (SimChain::new(), store());
    let mut e = engine(&sim, &st, opts(), ChannelKind::Public).await;
    let mut s = spec(Mode::NextBlockBestEffort);
    s.require_sim_ok = true;
    s.window_blocks = 50;
    e.add_task(s, quote(), NOW_S).unwrap();
    sim.set(|i| i.sim_ok = false); // pool has liquidity but the sale still reverts (trading closed)
    let log = liquidity_log(101, 48);
    e.on_event(log_event(log.clone(), 0)).await.unwrap();
    assert_eq!(sim.get(|i| i.accepted.len()), 0);

    e.on_event(ChainEvent::LogRemoved { log, source: 0 })
        .await
        .unwrap();
    sim.set(|i| i.sim_ok = true);
    e.on_event(head(102)).await.unwrap();
    e.on_event(head(103)).await.unwrap();
    assert_eq!(
        sim.get(|i| i.accepted.len()),
        0,
        "a reverted launch must not trigger a sale"
    );
    assert_eq!(e.task(TASK).unwrap().state, St::Ready);

    e.on_event(log_event(liquidity_log(104, 9), 0))
        .await
        .unwrap(); // re-included later
    assert_eq!(sim.get(|i| i.accepted.len()), 1);
}

#[tokio::test]
async fn reorg_of_the_sale_reopens_task_and_budget() {
    let (sim, st) = (SimChain::new(), store());
    let mut e = engine(&sim, &st, opts(), ChannelKind::Public).await;
    e.add_task(spec(Mode::NextBlockBestEffort), quote(), NOW_S)
        .unwrap();
    sim.set(|i| i.opened = true);
    e.on_event(log_event(liquidity_log(100, 48), 0))
        .await
        .unwrap();
    let hash = sim.get(|i| i.accepted[0]);
    let raw = sim.raw_of(hash).unwrap();
    e.on_event(sim.mine(None, 0)).await.unwrap();
    assert_eq!(e.task(TASK).unwrap().state, St::Confirmed);

    sim.reorg_out(hash, &raw);
    sim.set(|i| i.finalized = 200);
    e.on_event(head(102)).await.unwrap();
    let t = e.task(TASK).unwrap();
    assert_eq!(t.state, St::Triggered, "confirmation withdrawn");
    assert_eq!((t.budget.spent(), t.budget.reserved()), (0, MAX_COST));
    assert_eq!(
        sim.get(|i| i.accepted.len()),
        1,
        "no second transaction is signed for the same sale"
    );

    sim.set(|i| i.block = 102);
    e.on_event(sim.mine(None, 0)).await.unwrap();
    e.on_event(head(104)).await.unwrap(); // finality check
    let t = e.task(TASK).unwrap();
    assert_eq!(t.state, St::Confirmed);
    assert_eq!(t.budget.spent(), GAS_OK as u128 * PRICE, "counted once");
    assert_eq!(st.attempts_in_state(&["final_ok"]).unwrap().len(), 1);
}

#[tokio::test]
async fn broadcast_timeout_is_not_a_failure() {
    let (sim, st) = (SimChain::new(), store());
    let mut e = engine(&sim, &st, opts(), ChannelKind::Public).await;
    let mut s = spec(Mode::NextBlockBestEffort);
    s.max_attempts = 3;
    s.window_blocks = 50;
    e.add_task(s, quote(), NOW_S).unwrap();
    sim.set(|i| {
        i.opened = true;
        i.send_mode = Some(SendMode::TimeoutButAccept);
    });
    e.on_event(log_event(liquidity_log(100, 48), 0))
        .await
        .unwrap();
    assert_eq!(st.attempts_in_state(&["unknown"]).unwrap().len(), 1);
    assert_eq!(
        e.task(TASK).unwrap().budget.reserved(),
        MAX_COST,
        "still reserved: it may land"
    );
    e.on_event(head(100)).await.unwrap();
    e.on_event(head(100)).await.unwrap();
    assert_eq!(
        sim.count("eth_sendRawTransaction"),
        1,
        "no blind re-send while the first one is open"
    );
    e.on_event(sim.mine(None, 0)).await.unwrap();
    assert_eq!(e.task(TASK).unwrap().state, St::Confirmed);
    assert_eq!(st.attempts_in_state(&["included_ok"]).unwrap().len(), 1);
}

#[tokio::test]
async fn explicit_rejection_releases_budget_and_nonce() {
    let (sim, st) = (SimChain::new(), store());
    let mut e = engine(&sim, &st, opts(), ChannelKind::Public).await;
    e.add_task(spec(Mode::NextBlockBestEffort), quote(), NOW_S)
        .unwrap();
    sim.set(|i| i.send_mode = Some(SendMode::Reject));
    e.on_event(log_event(liquidity_log(100, 48), 0))
        .await
        .unwrap();
    assert_eq!(e.task(TASK).unwrap().budget.exposure(), 0);
    assert_eq!(st.attempts_in_state(&["rejected"]).unwrap()[0].nonce, 5);
    assert_eq!(e.inflight_count(), 0);
}

#[tokio::test]
async fn nonce_taken_by_another_transaction_then_retry() {
    let (sim, st) = (SimChain::new(), store());
    let mut e = engine(&sim, &st, opts(), ChannelKind::Public).await;
    let mut s = spec(Mode::NextBlockBestEffort);
    s.max_attempts = 2;
    s.window_blocks = 50;
    e.add_task(s, quote(), NOW_S).unwrap();
    sim.set(|i| i.opened = true);
    e.on_event(log_event(liquidity_log(100, 48), 0))
        .await
        .unwrap();
    sim.foreign_nonce_use(); // nonce 5 consumed elsewhere, our tx can never land
    for _ in 0..3 {
        e.on_event(head(100)).await.unwrap();
    }
    let replaced = st.attempts_in_state(&["replaced"]).unwrap();
    assert_eq!((replaced.len(), replaced[0].nonce), (1, 5));
    assert_eq!(
        sim.get(|i| i.accepted.len()),
        2,
        "second attempt signed with the next nonce"
    );
    assert_eq!(
        e.task(TASK).unwrap().budget.reserved(),
        MAX_COST,
        "only the live nonce is reserved"
    );
    e.on_event(sim.mine(None, 0)).await.unwrap();
    let t = e.task(TASK).unwrap();
    assert_eq!(t.state, St::Confirmed);
    assert_eq!(t.budget.spent(), GAS_OK as u128 * PRICE);
    assert_eq!(st.attempts_in_state(&["included_ok"]).unwrap()[0].nonce, 6);
}

#[tokio::test]
async fn restart_recovers_open_transaction() {
    let (sim, st) = (SimChain::new(), store());
    {
        let mut e = engine(&sim, &st, opts(), ChannelKind::Public).await;
        e.add_task(spec(Mode::NextBlockBestEffort), quote(), NOW_S)
            .unwrap();
        sim.set(|i| i.opened = true);
        e.on_event(log_event(liquidity_log(100, 48), 0))
            .await
            .unwrap();
    } // process dies with one transaction in the mempool

    let mut e = engine(&sim, &st, opts(), ChannelKind::Public).await;
    e.add_task(spec(Mode::NextBlockBestEffort), None, NOW_S + 10_000)
        .unwrap(); // cap comes from the DB, no new quote
    e.recover().await.unwrap();
    assert_eq!(e.inflight_count(), 1);
    let t = e.task(TASK).unwrap();
    assert_eq!(
        (t.state, t.sent, t.budget.reserved()),
        (St::Triggered, 1, MAX_COST)
    );

    e.on_event(log_event(liquidity_log(100, 48), 0))
        .await
        .unwrap(); // launch log replayed after restart
    assert_eq!(
        sim.get(|i| i.accepted.len()),
        1,
        "no second sale after restart"
    );
    e.on_event(sim.mine(None, 0)).await.unwrap();
    assert_eq!(e.task(TASK).unwrap().state, St::Confirmed);
    assert_eq!(
        st.task(TASK).unwrap().unwrap().spent_wei,
        GAS_OK as u128 * PRICE
    );

    // a third start finds a finished task and does nothing
    let mut e = engine(&sim, &st, opts(), ChannelKind::Public).await;
    e.add_task(spec(Mode::NextBlockBestEffort), None, NOW_S)
        .unwrap();
    e.recover().await.unwrap();
    e.on_event(log_event(liquidity_log(100, 48), 0))
        .await
        .unwrap();
    assert_eq!(sim.get(|i| i.accepted.len()), 1);
}

#[tokio::test]
async fn budget_reserves_charges_failures_and_refuses_overrun() {
    let (sim, st) = (SimChain::new(), store());
    let mut o = opts();
    o.usd_micros = 12_000; // cap = 1.9e13 wei at 600 USDT/BNB with the 5% buffer
    let mut e = engine(&sim, &st, o, ChannelKind::Public).await;
    let mut s = spec(Mode::PreArmedSpray);
    s.max_attempts = 10;
    s.window_blocks = 50;
    e.add_task(s, quote(), NOW_S).unwrap();
    assert_eq!(e.task(TASK).unwrap().budget.cap(), 19_000_000_000_000);
    e.arm(TASK).unwrap();

    for _ in 0..5 {
        e.on_event(sim.mine(None, 0)).await.unwrap(); // launch never happens: every attempt reverts
    }
    let t = e.task(TASK).unwrap();
    let fail = GAS_FAIL_PRE as u128 * PRICE;
    assert_eq!(
        sim.get(|i| i.accepted.len()),
        2,
        "third attempt would exceed the cap and is never signed away"
    );
    assert_eq!(
        t.budget.spent(),
        2 * fail,
        "failed transactions are charged"
    );
    assert_eq!(t.state, St::Stopped);
    assert!(t.reason.contains("budget exceeded"), "{}", t.reason);
    assert!(t.budget.exposure() <= t.budget.cap());
    assert!(
        t.budget.remaining() > 0,
        "leftover budget is not spent for its own sake"
    );
}

#[tokio::test]
async fn stale_or_missing_price_refuses_to_arm() {
    let (sim, st) = (SimChain::new(), store());
    let mut e = engine(&sim, &st, opts(), ChannelKind::Public).await;
    let stale = quote().map(|mut q| {
        q.fetched_at_s = NOW_S - 31;
        q
    });
    let err = e
        .add_task(spec(Mode::NextBlockBestEffort), stale, NOW_S)
        .unwrap_err();
    assert!(err.to_string().contains("stale"), "{err}");
    assert!(e
        .add_task(spec(Mode::NextBlockBestEffort), None, NOW_S)
        .is_err());
    assert!(e.tasks.is_empty() && st.task(TASK).unwrap().is_none());
    e.on_event(log_event(liquidity_log(101, 48), 0))
        .await
        .unwrap();
    assert_eq!(sim.get(|i| i.accepted.len()), 0);
}

fn strict_spec() -> shooter::config::TaskSpec {
    let mut s = spec(Mode::StrictSameBlock);
    s.strict_targets = vec![PendingRule {
        to: PROBE,
        selector: fixed_bytes!("adbbadec"),
        from: None,
    }];
    s
}

#[tokio::test]
async fn strict_mode_is_refused_without_the_capability() {
    let (sim, st) = (SimChain::new(), store());
    for kind in [ChannelKind::Public, ChannelKind::Club48] {
        let mut e = engine(&sim, &st, opts(), kind).await;
        let err = e
            .add_task(strict_spec(), quote(), NOW_S)
            .unwrap_err()
            .to_string();
        assert!(err.contains("refused"), "{err}");
    }
    let mut o = opts();
    o.has_pending_source = false;
    let mut e = engine(&sim, &st, o, ChannelKind::Blockrazor).await;
    assert!(e
        .add_task(strict_spec(), quote(), NOW_S)
        .unwrap_err()
        .to_string()
        .contains("no pending-transaction source"));
    assert!(st.task(TASK).unwrap().is_none());
}

#[tokio::test]
async fn strict_mode_never_downgrades() {
    let (sim, st) = (SimChain::new(), store());
    let mut e = engine(&sim, &st, opts(), ChannelKind::Blockrazor).await;
    e.add_task(strict_spec(), quote(), NOW_S).unwrap();

    // The launch lands without us ever seeing a usable target.
    sim.set(|i| i.opened = true);
    e.on_event(log_event(liquidity_log(101, 48), 0))
        .await
        .unwrap();
    for n in 101..110 {
        e.on_event(head(n)).await.unwrap();
    }
    let unusable = PendingTx {
        hash: B256::repeat_byte(1),
        from: PROBE,
        to: Some(PROBE),
        input: Bytes::from(vec![0xad, 0xbb, 0xad, 0xec]),
        raw: None,
    };
    e.on_event(ChainEvent::Pending {
        tx: unusable,
        source: 0,
        seen_ms: 1,
    })
    .await
    .unwrap();
    assert_eq!(
        (
            sim.count("eth_sendRawTransaction"),
            sim.count("eth_sendPrivateTransaction"),
            sim.count("eth_sendBundle")
        ),
        (0, 0, 0)
    );
    assert_eq!(e.task(TASK).unwrap().state, St::Ready);

    // A real target with raw bytes: one bundle, target first, our sale last, bounded by maxBlockNumber.
    let target = PendingTx {
        hash: B256::repeat_byte(2),
        from: PROBE,
        to: Some(PROBE),
        input: Bytes::from(vec![0xad, 0xbb, 0xad, 0xec, 0, 0]),
        raw: Some(Bytes::from(vec![0xf8, 0x01])),
    };
    e.on_event(ChainEvent::Pending {
        tx: target,
        source: 0,
        seen_ms: 1,
    })
    .await
    .unwrap();
    let bundle = sim.get(|i| i.bundles.clone());
    assert_eq!(bundle.len(), 1);
    assert_eq!(bundle[0]["txs"].as_array().unwrap().len(), 2);
    assert_eq!(bundle[0]["txs"][0], "0xf801");
    assert_eq!(bundle[0]["maxBlockNumber"], 109 + 3);
    assert!(bundle[0].get("revertingTxHashes").is_none());

    // The builder never includes it. The task ends; nothing is re-sent as a plain transaction.
    for n in 110..120 {
        e.on_event(head(n)).await.unwrap();
    }
    assert_eq!(e.task(TASK).unwrap().state, St::Expired);
    assert_eq!(
        (
            sim.count("eth_sendRawTransaction"),
            sim.count("eth_sendPrivateTransaction"),
            sim.count("eth_sendBundle")
        ),
        (0, 0, 1)
    );
}

#[tokio::test]
async fn spray_lands_in_the_launch_block_after_the_launch() {
    let (sim, st) = (SimChain::new(), store());
    let mut e = engine(&sim, &st, opts(), ChannelKind::Public).await;
    let mut s = spec(Mode::PreArmedSpray);
    s.max_attempts = 10;
    s.window_blocks = 20;
    s.arm_probe = Some(Probe {
        to: PROBE,
        data: Bytes::new(),
        min_value: alloy::primitives::U256::from(7990u64),
    });
    e.add_task(s, quote(), NOW_S).unwrap();

    sim.set(|i| i.probe_value = 7000);
    e.on_event(sim.mine(None, 0)).await.unwrap();
    assert_eq!(
        (e.task(TASK).unwrap().state, sim.get(|i| i.accepted.len())),
        (St::Ready, 0),
        "not armed below the threshold"
    );

    sim.set(|i| i.probe_value = 7995);
    e.on_event(sim.mine(None, 0)).await.unwrap(); // arms
    e.on_event(sim.mine(None, 0)).await.unwrap(); // attempt 1 queued
    e.on_event(sim.mine(None, 0)).await.unwrap(); // attempt 1 reverts (no pool yet), attempt 2 queued
    let h = sim.mine(Some(48), 108); // launch at index 48, attempt 2 at index 108
    let launch_block = sim.get(|i| i.block);
    e.on_event(h).await.unwrap();
    e.on_event(log_event(liquidity_log(launch_block, 48), 0))
        .await
        .unwrap();

    let t = e.task(TASK).unwrap();
    assert_eq!(t.state, St::Confirmed);
    assert_eq!(t.placement, Some(Placement::SameBlockAfter));
    assert_eq!(sim.get(|i| i.accepted.len()), 2);
    assert_eq!(t.budget.spent(), (GAS_FAIL_PRE + GAS_OK) as u128 * PRICE);
    assert!(st
        .metrics(TASK)
        .unwrap()
        .contains(&("same_block_after_launch".to_string(), 1)));
    for _ in 0..3 {
        e.on_event(sim.mine(None, 0)).await.unwrap();
    }
    assert_eq!(
        sim.get(|i| i.accepted.len()),
        2,
        "spraying stops after the sale"
    );
}

#[tokio::test]
async fn spray_ordered_before_the_launch_falls_to_the_next_block() {
    let (sim, st) = (SimChain::new(), store());
    let mut e = engine(&sim, &st, opts(), ChannelKind::Public).await;
    let mut s = spec(Mode::PreArmedSpray);
    s.max_attempts = 10;
    s.window_blocks = 20;
    e.add_task(s, quote(), NOW_S).unwrap();
    e.arm(TASK).unwrap();
    e.on_event(sim.mine(None, 0)).await.unwrap();
    let h = sim.mine(Some(48), 38); // our transaction sits before the launch in the same block
    let launch_block = sim.get(|i| i.block);
    e.on_event(log_event(liquidity_log(launch_block, 48), 0))
        .await
        .unwrap();
    e.on_event(h).await.unwrap();
    e.on_event(sim.mine(None, 0)).await.unwrap();
    let t = e.task(TASK).unwrap();
    assert_eq!(
        (t.state, t.placement),
        (St::Confirmed, Some(Placement::BlocksAfter(1)))
    );
}

#[tokio::test]
async fn dry_run_signs_but_never_broadcasts() {
    let (sim, st) = (SimChain::new(), store());
    let mut o = opts();
    o.dry_run = true;
    let mut e = engine(&sim, &st, o, ChannelKind::Public).await;
    e.add_task(spec(Mode::NextBlockBestEffort), quote(), NOW_S)
        .unwrap();
    e.on_event(log_event(liquidity_log(101, 48), 0))
        .await
        .unwrap();
    assert_eq!(sim.count("eth_sendRawTransaction"), 0);
    assert_eq!(st.attempts_in_state(&["dry_run"]).unwrap().len(), 1);
    assert_eq!(e.task(TASK).unwrap().budget.exposure(), 0);
}
