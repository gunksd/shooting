//! Everything that can be prepared before the trigger: static calldata, pre-signed transactions,
//! and the validity checks (allowance, balance, on-chain task, executor funds).
//!
//! Pre-signable: to, calldata (taskId, amountIn, minOut), gas limit, gas price, chain id, nonce.
//! Only known after the trigger: which block it lands in, and for strict mode the target bytes.
use crate::budget::PriceQuote;
use crate::config::TaskSpec;
use crate::dex_adapter::{allowanceCall, balanceOfCall, getReservesCall, getTaskCall, DexAdapter};
use crate::rpc::{self, ChainRpc};
use crate::signer::{ExecSigner, SignedTx};
use alloy::primitives::{Address, U256};
use alloy::sol;
use alloy::sol_types::SolCall;
use anyhow::{anyhow, Result};
use std::collections::HashMap;

sol! { function token0() returns (address); }

#[derive(Default)]
pub struct Planner {
    cache: HashMap<(alloy::primitives::B256, u64, u128), SignedTx>,
}

impl Planner {
    /// Returns the signed sale for `nonce`, signing at most once per (task, nonce, gas price).
    pub fn presign(
        &mut self,
        signer: &ExecSigner,
        spec: &TaskSpec,
        dex: &dyn DexAdapter,
        nonce: u64,
    ) -> Result<SignedTx> {
        let key = (spec.task_id, nonce, spec.gas_price_wei);
        if let Some(tx) = self.cache.get(&key) {
            return Ok(tx.clone());
        }
        let data = dex.sell_calldata(spec.task_id, spec.amount_in, spec.min_out);
        let tx = signer.sign(
            nonce,
            spec.gas_price_wei,
            spec.gas_limit,
            spec.executor_contract,
            U256::ZERO,
            data,
        )?;
        self.cache.insert(key, tx.clone());
        Ok(tx)
    }

    pub fn forget(&mut self, task: alloy::primitives::B256) {
        self.cache.retain(|k, _| k.0 != task);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub name: &'static str,
    pub ok: bool,
    pub detail: String,
}

/// Reads chain state and reports every precondition. Nothing is assumed: a task only becomes
/// ready when all checks pass.
pub async fn preflight(
    rpc: &dyn ChainRpc,
    spec: &TaskSpec,
    executor: Address,
    cap_wei: u128,
) -> Result<Vec<Check>> {
    let mut out = vec![];
    let call = |to: Address, data: Vec<u8>| async move {
        rpc::eth_call(rpc, None, to, &data.into()).await
    };

    let allowance = U256::from_be_slice(
        &call(
            spec.token_in,
            allowanceCall {
                owner: spec.owner,
                spender: spec.executor_contract,
            }
            .abi_encode(),
        )
        .await?,
    );
    out.push(Check {
        name: "allowance",
        ok: allowance >= spec.amount_in,
        detail: format!("{allowance} (need {})", spec.amount_in),
    });

    let bal = U256::from_be_slice(
        &call(
            spec.token_in,
            balanceOfCall { who: spec.owner }.abi_encode(),
        )
        .await?,
    );
    out.push(Check {
        name: "owner_token_balance",
        ok: bal >= spec.amount_in,
        detail: format!("{bal} (need {})", spec.amount_in),
    });

    let raw = call(
        spec.executor_contract,
        getTaskCall {
            taskId: spec.task_id,
        }
        .abi_encode(),
    )
    .await?;
    let task = getTaskCall::abi_decode_returns(&raw).map_err(|e| anyhow!("getTask decode: {e}"))?;
    let p = &task.p;
    let floor = (p.minOutFloor * spec.amount_in).div_ceil(p.maxAmountIn.max(U256::from(1u64)));
    let checks = [
        (
            "task_open",
            task.status == 1,
            format!("status {}", task.status),
        ),
        (
            "task_owner",
            task.owner == spec.owner,
            format!("{}", task.owner),
        ),
        (
            "task_executor",
            p.executor == executor,
            format!("{}", p.executor),
        ),
        (
            "task_tokens",
            p.tokenIn == spec.token_in && p.tokenOut == spec.token_out && p.factory == spec.factory,
            format!("{} -> {}", p.tokenIn, p.tokenOut),
        ),
        (
            "task_amount",
            spec.amount_in <= p.maxAmountIn,
            format!("max {}", p.maxAmountIn),
        ),
        (
            "task_floor",
            spec.min_out >= floor,
            format!(
                "on-chain floor for this amount {floor}, configured {}",
                spec.min_out
            ),
        ),
    ];
    out.extend(
        checks
            .into_iter()
            .map(|(name, ok, detail)| Check { name, ok, detail }),
    );

    let bnb = rpc::balance(rpc, executor).await?;
    out.push(Check {
        name: "executor_bnb",
        ok: bnb >= U256::from(cap_wei),
        detail: format!("{bnb} wei (budget cap {cap_wei})"),
    });

    let one = spec.gas_limit as u128 * spec.gas_price_wei;
    out.push(Check {
        name: "attempt_fits_budget",
        ok: one <= cap_wei,
        detail: format!("worst case per attempt {one} wei, cap {cap_wei}"),
    });
    Ok(out)
}

/// BNB price in micro-USDT from a V2 pair's reserves (both tokens 18 decimals on BSC).
/// Spot reserves can be moved within a block; callers must apply sanity bounds (`PricePolicy`).
pub async fn fetch_bnb_price(
    rpc: &dyn ChainRpc,
    pair: Address,
    stable: Address,
    now_s: u64,
) -> Result<PriceQuote> {
    let t0 = rpc::eth_call(rpc, None, pair, &token0Call {}.abi_encode().into()).await?;
    let token0 = Address::from_slice(t0.get(12..32).ok_or_else(|| anyhow!("token0 decode"))?);
    let r = getReservesCall::abi_decode_returns(
        &rpc::eth_call(rpc, None, pair, &getReservesCall {}.abi_encode().into()).await?,
    )
    .map_err(|e| anyhow!("getReserves decode: {e}"))?;
    let (r0, r1) = (U256::from(r.r0), U256::from(r.r1));
    let (stable_r, bnb_r) = if token0 == stable { (r0, r1) } else { (r1, r0) };
    if bnb_r.is_zero() {
        return Err(anyhow!("price pair has no reserves"));
    }
    let micros = stable_r * U256::from(1_000_000u64) / bnb_r;
    Ok(PriceQuote {
        price_micros: u64::try_from(micros).map_err(|_| anyhow!("price overflow"))?,
        fetched_at_s: now_s,
    })
}
