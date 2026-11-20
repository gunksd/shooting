//! Launch detection. An adapter turns raw logs / pending transactions into launch signals and is
//! the only place that decides whether an event really belongs to the watched pool.
use crate::config::PendingRule;
use crate::dex_adapter::{sort_tokens, PairCreated, Sync};
use crate::types::{Log, PendingTx};
use alloy::primitives::{Address, B256};
use alloy::sol_types::SolEvent;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchSignal {
    /// Pool contract created. Not tradable by itself.
    PoolCreated { block: u64 },
    /// Both reserves became non-zero: liquidity is in. Whether transfers are enabled is a separate
    /// question answered by simulation (tokens with an `openTrading` switch).
    Liquidity {
        block: u64,
        tx_index: u64,
        tx_hash: B256,
    },
    /// The log that produced `Liquidity` was removed by a reorg.
    Reverted { tx_hash: B256 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Idle,
    PoolCreated,
    Liquid,
}

pub trait LaunchAdapter: Send {
    /// Contracts whose logs must be subscribed.
    fn addresses(&self) -> Vec<Address>;
    fn on_log(&mut self, log: &Log) -> Option<LaunchSignal>;
    fn on_removed(&mut self, log: &Log) -> Option<LaunchSignal>;
    fn phase(&self) -> Phase;
}

/// V2-style launch: PairCreated on the factory, then the first Sync with two non-zero reserves on
/// the pair. Works for "create pair + add liquidity" in one or several transactions and for
/// launchpad migrations that end in a V2 pool.
pub struct V2Launch {
    factory: Address,
    pair: Address,
    token0: Address,
    token1: Address,
    phase: Phase,
    liquidity_tx: Option<B256>,
}

impl V2Launch {
    pub fn new(factory: Address, pair: Address, token_a: Address, token_b: Address) -> Self {
        let (token0, token1) = sort_tokens(token_a, token_b);
        Self {
            factory,
            pair,
            token0,
            token1,
            phase: Phase::Idle,
            liquidity_tx: None,
        }
    }
}

impl LaunchAdapter for V2Launch {
    fn addresses(&self) -> Vec<Address> {
        vec![self.factory, self.pair]
    }

    fn phase(&self) -> Phase {
        self.phase
    }

    fn on_log(&mut self, log: &Log) -> Option<LaunchSignal> {
        let topic0 = *log.topics.first()?;
        // The emitting address is set by the EVM and cannot be forged: an attacker contract can emit
        // a look-alike event but never from the real factory or the real pair address.
        if log.address == self.factory && topic0 == PairCreated::SIGNATURE_HASH {
            let ev = PairCreated::decode_raw_log(log.topics.iter().copied(), &log.data).ok()?;
            if ev.token0 != self.token0 || ev.token1 != self.token1 || ev.pair != self.pair {
                return None; // some other pool of the same factory
            }
            if self.phase == Phase::Idle {
                self.phase = Phase::PoolCreated;
                return Some(LaunchSignal::PoolCreated {
                    block: log.block_number,
                });
            }
            return None;
        }
        if log.address == self.pair && topic0 == Sync::SIGNATURE_HASH {
            let ev = Sync::decode_raw_log(log.topics.iter().copied(), &log.data).ok()?;
            if ev.reserve0.is_zero() || ev.reserve1.is_zero() || self.phase == Phase::Liquid {
                return None;
            }
            self.phase = Phase::Liquid;
            self.liquidity_tx = Some(log.transaction_hash);
            return Some(LaunchSignal::Liquidity {
                block: log.block_number,
                tx_index: log.transaction_index,
                tx_hash: log.transaction_hash,
            });
        }
        None
    }

    fn on_removed(&mut self, log: &Log) -> Option<LaunchSignal> {
        if self.phase == Phase::Liquid
            && self.liquidity_tx == Some(log.transaction_hash)
            && log.address == self.pair
        {
            self.phase = Phase::PoolCreated;
            self.liquidity_tx = None;
            return Some(LaunchSignal::Reverted {
                tx_hash: log.transaction_hash,
            });
        }
        None
    }
}

pub fn rule_matches(rule: &PendingRule, tx: &PendingTx) -> bool {
    tx.to == Some(rule.to)
        && tx.input.len() >= 4
        && tx.input[..4] == rule.selector[..]
        && rule.from.is_none_or(|f| f == tx.from)
}

/// Collects the pending transactions a strict same-block bundle depends on. Every rule must be
/// matched (in rule order) before a bundle may be built; one launch can need several transactions.
#[derive(Debug, Default)]
pub struct TargetSet {
    rules: Vec<PendingRule>,
    matched: Vec<Option<PendingTx>>,
}

impl TargetSet {
    pub fn new(rules: Vec<PendingRule>) -> Self {
        let n = rules.len();
        Self {
            rules,
            matched: vec![None; n],
        }
    }

    /// Returns the complete ordered target list once all rules are matched with raw bytes available.
    pub fn offer(&mut self, tx: &PendingTx) -> Option<Vec<PendingTx>> {
        tx.raw.as_ref()?;
        let i = self.rules.iter().position(|r| rule_matches(r, tx))?;
        if self.matched[i].is_none() {
            self.matched[i] = Some(tx.clone());
        }
        self.matched.iter().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{address, fixed_bytes, Bytes, U256};
    use alloy::sol_types::SolValue;

    const FACTORY: Address = address!("cA143Ce32Fe78f1f7019d7d551a6402fC5350c73");
    const PAIR: Address = address!("82b2c3f6DF175fC4644d1A27D72434a1322d26c9");
    const WBNB: Address = address!("bb4CdB9CBd36B01bD1cBaEBF2De08d9173bc095c");
    const TOKEN: Address = address!("EbE3069bC7B5F1602De421f7a1089759F15AB636");

    fn log(address: Address, topics: Vec<B256>, data: Vec<u8>, tx: u8) -> Log {
        Log {
            address,
            topics,
            data: data.into(),
            block_number: 100,
            block_hash: B256::repeat_byte(9),
            transaction_hash: B256::repeat_byte(tx),
            transaction_index: 48,
            log_index: 1,
            removed: false,
        }
    }
    fn pair_created(from: Address, pair: Address, t1: Address) -> Log {
        log(
            from,
            vec![
                PairCreated::SIGNATURE_HASH,
                WBNB.into_word(),
                t1.into_word(),
            ],
            (pair, U256::from(1u64)).abi_encode(),
            1,
        )
    }
    fn sync(from: Address, r0: u64, r1: u64, tx: u8) -> Log {
        log(
            from,
            vec![Sync::SIGNATURE_HASH],
            (U256::from(r0), U256::from(r1)).abi_encode(),
            tx,
        )
    }

    #[test]
    fn full_launch_sequence() {
        let mut a = V2Launch::new(FACTORY, PAIR, TOKEN, WBNB);
        assert_eq!(
            a.on_log(&pair_created(FACTORY, PAIR, TOKEN)),
            Some(LaunchSignal::PoolCreated { block: 100 })
        );
        assert_eq!(
            a.on_log(&sync(PAIR, 0, 0, 2)),
            None,
            "empty pool is not a launch"
        );
        assert_eq!(
            a.on_log(&sync(PAIR, 18, 200, 2)),
            Some(LaunchSignal::Liquidity {
                block: 100,
                tx_index: 48,
                tx_hash: B256::repeat_byte(2)
            })
        );
        assert_eq!(
            a.on_log(&sync(PAIR, 17, 201, 3)),
            None,
            "later syncs do not re-trigger"
        );
    }

    #[test]
    fn forged_or_unrelated_events_are_ignored() {
        let mut a = V2Launch::new(FACTORY, PAIR, TOKEN, WBNB);
        let evil = address!("00000000000000000000000000000000000bad00");
        assert_eq!(
            a.on_log(&pair_created(evil, PAIR, TOKEN)),
            None,
            "wrong emitter"
        );
        assert_eq!(
            a.on_log(&pair_created(FACTORY, evil, TOKEN)),
            None,
            "wrong pair address"
        );
        assert_eq!(
            a.on_log(&pair_created(FACTORY, PAIR, evil)),
            None,
            "other token"
        );
        assert_eq!(
            a.on_log(&sync(evil, 18, 200, 2)),
            None,
            "Sync from another contract"
        );
        assert_eq!(a.phase(), Phase::Idle);
    }

    #[test]
    fn reorg_resets_liquidity() {
        let mut a = V2Launch::new(FACTORY, PAIR, TOKEN, WBNB);
        a.on_log(&pair_created(FACTORY, PAIR, TOKEN));
        let s = sync(PAIR, 18, 200, 2);
        a.on_log(&s);
        assert_eq!(a.on_removed(&sync(PAIR, 18, 200, 7)), None, "different tx");
        assert_eq!(
            a.on_removed(&s),
            Some(LaunchSignal::Reverted {
                tx_hash: B256::repeat_byte(2)
            })
        );
        assert!(
            matches!(a.on_log(&s), Some(LaunchSignal::Liquidity { .. })),
            "can trigger again after re-inclusion"
        );
    }

    #[test]
    fn multi_transaction_targets_need_all_rules_and_raw_bytes() {
        let sale = address!("13406cE228a4c7A32699b384ECe4da874dc407D1");
        let rules = vec![
            PendingRule {
                to: sale,
                selector: fixed_bytes!("adbbadec"),
                from: None,
            },
            PendingRule {
                to: TOKEN,
                selector: fixed_bytes!("c9567bf9"),
                from: None,
            },
        ];
        let mut set = TargetSet::new(rules);
        let mk = |to, sel: [u8; 4], raw: bool, h: u8| PendingTx {
            hash: B256::repeat_byte(h),
            from: evil(),
            to: Some(to),
            input: Bytes::from(sel.to_vec()),
            raw: raw.then(|| Bytes::from(vec![h])),
        };
        fn evil() -> Address {
            address!("00000000000000000000000000000000000bad00")
        }
        assert!(
            set.offer(&mk(sale, [0xad, 0xbb, 0xad, 0xec], false, 1))
                .is_none(),
            "no raw bytes, not usable"
        );
        assert!(
            set.offer(&mk(sale, [0xad, 0xbb, 0xad, 0xec], true, 1))
                .is_none(),
            "second dependency missing"
        );
        assert!(set.offer(&mk(sale, [1, 2, 3, 4], true, 5)).is_none());
        let all = set
            .offer(&mk(TOKEN, [0xc9, 0x56, 0x7b, 0xf9], true, 2))
            .unwrap();
        assert_eq!(
            all.iter().map(|t| t.hash).collect::<Vec<_>>(),
            vec![B256::repeat_byte(1), B256::repeat_byte(2)]
        );
    }
}
