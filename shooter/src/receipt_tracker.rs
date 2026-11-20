//! Turns receipts into facts: did it succeed, what was really sold and received, what did it cost,
//! and where did it land relative to the launch transaction.
use crate::dex_adapter::TaskExecuted;
use crate::types::Receipt;
use alloy::primitives::{Address, B256, U256};
use alloy::sol_types::SolEvent;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub success: bool,
    pub block: u64,
    pub block_hash: B256,
    pub tx_index: u64,
    pub gas_used: u64,
    pub cost_wei: u128,
    /// From the contract's TaskExecuted event (balance deltas measured on chain), only on success.
    pub actual_in: Option<U256>,
    pub received: Option<U256>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// Same block as the launch and after it: the target of strict_same_block.
    SameBlockAfter,
    /// Same block but ordered before the launch transaction.
    SameBlockBefore,
    /// `n` blocks after the launch (1 = next block).
    BlocksAfter(u64),
    BeforeLaunch,
}

pub fn classify(
    r: &Receipt,
    task_id: B256,
    executor_contract: Address,
    signed_gas_price: u128,
) -> Outcome {
    let price = r
        .effective_gas_price
        .and_then(|p| u128::try_from(p).ok())
        .unwrap_or(signed_gas_price);
    let ev = r.logs.iter().find_map(|l| {
        if l.address != executor_contract
            || l.topics.first() != Some(&TaskExecuted::SIGNATURE_HASH)
            || l.topics.get(1) != Some(&task_id)
        {
            return None;
        }
        TaskExecuted::decode_raw_log(l.topics.iter().copied(), &l.data).ok()
    });
    // A status-1 receipt without our event is not a sale (e.g. wrong contract); never count it.
    let success = r.status == Some(1) && ev.is_some();
    Outcome {
        success,
        block: r.block_number,
        block_hash: r.block_hash,
        tx_index: r.transaction_index,
        gas_used: r.gas_used,
        cost_wei: r.gas_used as u128 * price,
        actual_in: ev.as_ref().filter(|_| success).map(|e| e.actualIn),
        received: ev.as_ref().filter(|_| success).map(|e| e.received),
    }
}

pub fn placement(open_block: u64, open_tx_index: u64, block: u64, tx_index: u64) -> Placement {
    match block.cmp(&open_block) {
        std::cmp::Ordering::Less => Placement::BeforeLaunch,
        std::cmp::Ordering::Greater => Placement::BlocksAfter(block - open_block),
        std::cmp::Ordering::Equal if tx_index > open_tx_index => Placement::SameBlockAfter,
        std::cmp::Ordering::Equal => Placement::SameBlockBefore,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Log;
    use alloy::primitives::address;
    use alloy::sol_types::SolValue;

    const EX: Address = address!("00000000000000000000000000000000000000e1");

    fn receipt(status: u64, logs: Vec<Log>) -> Receipt {
        Receipt {
            transaction_hash: B256::repeat_byte(1),
            block_number: 10,
            block_hash: B256::repeat_byte(2),
            transaction_index: 108,
            gas_used: 155_822,
            status: Some(status),
            effective_gas_price: Some(U256::from(150_000_000u64)),
            logs,
        }
    }
    fn executed(addr: Address, task: B256) -> Log {
        Log {
            address: addr,
            topics: vec![TaskExecuted::SIGNATURE_HASH, task],
            data: (U256::from(500u64), U256::from(500u64), U256::from(44u64))
                .abi_encode()
                .into(),
            block_number: 10,
            block_hash: B256::repeat_byte(2),
            transaction_hash: B256::repeat_byte(1),
            transaction_index: 108,
            log_index: 0,
            removed: false,
        }
    }

    #[test]
    fn success_needs_status_and_our_event() {
        let id = B256::repeat_byte(7);
        let ok = classify(&receipt(1, vec![executed(EX, id)]), id, EX, 1);
        assert!(ok.success);
        assert_eq!(
            (ok.actual_in, ok.received),
            (Some(U256::from(500u64)), Some(U256::from(44u64)))
        );
        assert_eq!(ok.cost_wei, 155_822u128 * 150_000_000);
        assert!(
            !classify(&receipt(1, vec![]), id, EX, 1).success,
            "status 1 without the event is not a sale"
        );
        assert!(
            !classify(
                &receipt(
                    1,
                    vec![executed(
                        address!("00000000000000000000000000000000000000ff"),
                        id
                    )]
                ),
                id,
                EX,
                1
            )
            .success
        );
        assert!(
            !classify(
                &receipt(1, vec![executed(EX, B256::repeat_byte(8))]),
                id,
                EX,
                1
            )
            .success,
            "other task"
        );
        let failed = classify(&receipt(0, vec![]), id, EX, 1);
        assert!(
            !failed.success && failed.cost_wei > 0,
            "reverted transactions still cost gas"
        );
    }

    #[test]
    fn placement_relative_to_launch() {
        assert_eq!(placement(100, 48, 100, 108), Placement::SameBlockAfter);
        assert_eq!(placement(100, 48, 100, 38), Placement::SameBlockBefore);
        assert_eq!(placement(100, 48, 101, 0), Placement::BlocksAfter(1));
        assert_eq!(placement(100, 48, 99, 5), Placement::BeforeLaunch);
    }
}
