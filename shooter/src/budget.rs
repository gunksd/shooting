//! Execution budget accounting. Integer wei only; no floats anywhere.
//!
//! Exposure = spent + Σ(per-nonce reservation). Transactions that share a nonce are mutually
//! exclusive on chain, so a nonce group reserves the *maximum* of its members, not their sum.
//! Transactions with different nonces can all land, so groups add up.
use alloy::primitives::B256;
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BudgetError {
    #[error("budget exceeded: exposure {exposure} + {extra} > cap {cap}")]
    Exceeded {
        exposure: u128,
        extra: u128,
        cap: u128,
    },
    #[error("price quote is stale ({age_s}s > {max_age_s}s)")]
    PriceStale { age_s: u64, max_age_s: u64 },
    #[error("price {price_micros} outside sanity bounds [{min}, {max}]")]
    PriceOutOfBounds {
        price_micros: u64,
        min: u64,
        max: u64,
    },
}

/// BNB price in micro-USDT (1e-6 USDT) with the time it was read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PriceQuote {
    pub price_micros: u64,
    pub fetched_at_s: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct PricePolicy {
    pub max_age_s: u64,
    pub min_price_micros: u64,
    pub max_price_micros: u64,
    /// Haircut applied to the converted cap so a price move after arming is less likely to breach it.
    pub buffer_bps: u16,
}

/// Converts a USDT budget into a fixed wei cap. The cap is fixed in BNB at conversion time; its
/// later USD value moves with the BNB price and is not guaranteed.
pub fn cap_from_usd(
    usd_micros: u64,
    q: PriceQuote,
    now_s: u64,
    p: &PricePolicy,
) -> Result<u128, BudgetError> {
    let age = now_s.saturating_sub(q.fetched_at_s);
    if age > p.max_age_s {
        return Err(BudgetError::PriceStale {
            age_s: age,
            max_age_s: p.max_age_s,
        });
    }
    if q.price_micros < p.min_price_micros || q.price_micros > p.max_price_micros {
        return Err(BudgetError::PriceOutOfBounds {
            price_micros: q.price_micros,
            min: p.min_price_micros,
            max: p.max_price_micros,
        });
    }
    let wei = (usd_micros as u128) * 1_000_000_000_000_000_000u128 / (q.price_micros as u128);
    Ok(wei * (10_000 - p.buffer_bps.min(10_000) as u128) / 10_000)
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Group {
    txs: Vec<(B256, u128)>,
}

impl Group {
    fn max(&self) -> u128 {
        self.txs.iter().map(|t| t.1).max().unwrap_or(0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Budget {
    cap_wei: u128,
    spent_wei: u128,
    groups: BTreeMap<u64, Group>,
}

impl Budget {
    /// `already_spent` carries confirmed costs from earlier runs (and initialization costs when the
    /// operator chose to count them against the task budget).
    pub fn new(cap_wei: u128, already_spent: u128) -> Self {
        Self {
            cap_wei,
            spent_wei: already_spent,
            groups: BTreeMap::new(),
        }
    }

    pub fn cap(&self) -> u128 {
        self.cap_wei
    }
    pub fn spent(&self) -> u128 {
        self.spent_wei
    }
    pub fn reserved(&self) -> u128 {
        self.groups.values().map(Group::max).sum()
    }
    pub fn exposure(&self) -> u128 {
        self.spent_wei + self.reserved()
    }
    pub fn remaining(&self) -> u128 {
        self.cap_wei.saturating_sub(self.exposure())
    }

    /// Reserve the worst-case cost (gas_limit × gas_price + any value sent as tip) of a transaction
    /// before it is broadcast. Fails without side effects when the cap would be exceeded.
    pub fn try_reserve(
        &mut self,
        nonce: u64,
        hash: B256,
        max_cost: u128,
    ) -> Result<(), BudgetError> {
        let old = self.groups.get(&nonce).map(Group::max).unwrap_or(0);
        let extra = max_cost.saturating_sub(old);
        let exposure = self.exposure();
        if exposure + extra > self.cap_wei {
            return Err(BudgetError::Exceeded {
                exposure,
                extra,
                cap: self.cap_wei,
            });
        }
        let g = self.groups.entry(nonce).or_default();
        if !g.txs.iter().any(|t| t.0 == hash) {
            g.txs.push((hash, max_cost));
        }
        Ok(())
    }

    /// The nonce was consumed on chain. `actual_cost` is what our transaction paid (0 if a foreign
    /// transaction took the nonce). Failed transactions still pay gas and are settled the same way.
    pub fn settle(&mut self, nonce: u64, actual_cost: u128) {
        if self.groups.remove(&nonce).is_some() {
            self.spent_wei += actual_cost;
        }
    }

    /// Drop a reservation for a transaction that was never handed to any channel.
    pub fn release_unsent(&mut self, nonce: u64, hash: B256) {
        if let Some(g) = self.groups.get_mut(&nonce) {
            g.txs.retain(|t| t.0 != hash);
            if g.txs.is_empty() {
                self.groups.remove(&nonce);
            }
        }
    }

    /// A settled transaction left the canonical chain (reorg): its cost is no longer spent, but the
    /// signed transaction can be included again, so it goes back to being a reservation.
    pub fn reopen(&mut self, nonce: u64, hash: B256, max_cost: u128, settled_cost: u128) {
        self.spent_wei = self.spent_wei.saturating_sub(settled_cost);
        let g = self.groups.entry(nonce).or_default();
        if !g.txs.iter().any(|t| t.0 == hash) {
            g.txs.push((hash, max_cost));
        }
    }

    pub fn open_nonces(&self) -> Vec<u64> {
        self.groups.keys().copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const POLICY: PricePolicy = PricePolicy {
        max_age_s: 60,
        min_price_micros: 100_000_000,
        max_price_micros: 5_000_000_000,
        buffer_bps: 500,
    };

    fn h(n: u8) -> B256 {
        B256::repeat_byte(n)
    }

    #[test]
    fn usd_to_wei_cap_is_integer_and_buffered() {
        let q = PriceQuote {
            price_micros: 500_000_000,
            fetched_at_s: 1000,
        }; // 500 USDT/BNB
        let cap = cap_from_usd(500_000, q, 1010, &POLICY).unwrap(); // 0.5 USDT
        assert_eq!(cap, 950_000_000_000_000); // 0.001 BNB minus 5%
    }

    #[test]
    fn stale_or_absurd_price_stops() {
        let q = PriceQuote {
            price_micros: 500_000_000,
            fetched_at_s: 1000,
        };
        assert_eq!(
            cap_from_usd(500_000, q, 1061, &POLICY),
            Err(BudgetError::PriceStale {
                age_s: 61,
                max_age_s: 60
            })
        );
        let low = PriceQuote {
            price_micros: 1_000_000,
            fetched_at_s: 1000,
        };
        assert!(matches!(
            cap_from_usd(500_000, low, 1000, &POLICY),
            Err(BudgetError::PriceOutOfBounds { .. })
        ));
    }

    #[test]
    fn reserve_settle_and_reject() {
        let mut b = Budget::new(1_000, 0);
        b.try_reserve(7, h(1), 400).unwrap();
        b.try_reserve(8, h(2), 400).unwrap();
        assert_eq!(b.exposure(), 800);
        // a third distinct nonce could land too: must be refused, and leaves no trace
        assert!(matches!(
            b.try_reserve(9, h(3), 300),
            Err(BudgetError::Exceeded { .. })
        ));
        assert_eq!(b.exposure(), 800);
        // failed tx still costs gas
        b.settle(7, 120);
        assert_eq!((b.spent(), b.reserved(), b.remaining()), (120, 400, 480));
        b.try_reserve(9, h(3), 300).unwrap();
    }

    #[test]
    fn same_nonce_replacements_are_exclusive_not_additive() {
        let mut b = Budget::new(1_000, 0);
        b.try_reserve(7, h(1), 400).unwrap();
        b.try_reserve(7, h(2), 600).unwrap(); // replacement: only the delta is new exposure
        assert_eq!(b.reserved(), 600);
        assert!(b.try_reserve(7, h(3), 1_100).is_err());
        assert_eq!(b.reserved(), 600);
        b.settle(7, 450); // whichever landed, one cost
        assert_eq!((b.spent(), b.reserved()), (450, 0));
        b.settle(7, 450); // double settle is a no-op
        assert_eq!(b.spent(), 450);
    }

    #[test]
    fn reorg_turns_spent_back_into_reservation() {
        let mut b = Budget::new(1_000, 0);
        b.try_reserve(7, h(1), 400).unwrap();
        b.settle(7, 120);
        b.reopen(7, h(1), 400, 120);
        assert_eq!((b.spent(), b.reserved()), (0, 400));
    }

    #[test]
    fn leftover_is_not_spent_and_init_costs_can_count() {
        let mut b = Budget::new(1_000, 300); // init costs included by operator choice
        assert_eq!(b.remaining(), 700);
        assert!(b.try_reserve(1, h(1), 701).is_err());
        b.try_reserve(1, h(1), 700).unwrap();
        b.release_unsent(1, h(1));
        assert_eq!(b.remaining(), 700);
    }
}
