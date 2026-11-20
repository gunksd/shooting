//! DEX-specific pieces: pool address derivation, quoting, sale calldata.
//! Only PancakeSwap-V2-style pools are implemented. V3 or launchpad curves need their own
//! `DexAdapter` (and a matching on-chain executor); nothing here pretends to cover them.
use alloy::primitives::{keccak256, Address, Bytes, B256, U256};
use alloy::sol;
use alloy::sol_types::{SolCall, SolValue};

sol! {
    #[derive(Debug, PartialEq, Eq)]
    struct TaskParams {
        address executor; address tokenIn; address tokenOut; address router; address factory; address recipient;
        uint256 maxAmountIn; uint256 minOutFloor; uint64 deadline; uint64 minBlock; uint64 maxBlock;
        uint16 maxTransferLossBps; uint16 pairFeeBps; uint8 mode; bool nativeOut; address[] path;
    }
    #[derive(Debug, PartialEq, Eq)]
    struct Task { address owner; uint8 status; TaskParams p; }

    function createTask(TaskParams p) returns (bytes32);
    function cancelTask(bytes32 taskId);
    function execute(bytes32 taskId, uint256 amountIn, uint256 minOut);
    function getTask(bytes32 taskId) returns (Task);
    function nonces(address owner) returns (uint256);
    event TaskExecuted(bytes32 indexed taskId, uint256 amountIn, uint256 actualIn, uint256 received);

    function approve(address spender, uint256 amount) returns (bool);
    function allowance(address owner, address spender) returns (uint256);
    function balanceOf(address who) returns (uint256);
    function getReserves() returns (uint112 r0, uint112 r1, uint32 ts);
    event PairCreated(address indexed token0, address indexed token1, address pair, uint256 index);
    event Sync(uint112 reserve0, uint112 reserve1);
}

pub trait DexAdapter: Send + std::marker::Sync {
    fn name(&self) -> &'static str;
    /// Address of the pool the sale goes through; known before the pool exists.
    fn pool(&self) -> Address;
    fn sell_calldata(&self, task_id: B256, amount_in: U256, min_out: U256) -> Bytes;
    /// Output for `amount_in` given reserves, after the pool fee. Token transfer taxes are not modelled.
    fn quote(&self, amount_in: U256, reserve_in: U256, reserve_out: U256) -> U256;
    /// Splits the pair's (reserve0, reserve1) into (reserve_in, reserve_out).
    fn orient(&self, r0: U256, r1: U256) -> (U256, U256);
}

#[derive(Debug, Clone)]
pub struct PancakeV2 {
    pub factory: Address,
    pub init_code_hash: B256,
    pub token_in: Address,
    pub token_out: Address,
    pub fee_bps: u16,
}

pub fn sort_tokens(a: Address, b: Address) -> (Address, Address) {
    if a < b {
        (a, b)
    } else {
        (b, a)
    }
}

pub fn v2_pair_address(factory: Address, init_code_hash: B256, a: Address, b: Address) -> Address {
    let (t0, t1) = sort_tokens(a, b);
    let salt = keccak256([t0.as_slice(), t1.as_slice()].concat());
    factory.create2(salt, init_code_hash)
}

impl DexAdapter for PancakeV2 {
    fn name(&self) -> &'static str {
        "pancake_v2"
    }
    fn pool(&self) -> Address {
        v2_pair_address(
            self.factory,
            self.init_code_hash,
            self.token_in,
            self.token_out,
        )
    }
    fn sell_calldata(&self, task_id: B256, amount_in: U256, min_out: U256) -> Bytes {
        executeCall {
            taskId: task_id,
            amountIn: amount_in,
            minOut: min_out,
        }
        .abi_encode()
        .into()
    }
    fn quote(&self, amount_in: U256, reserve_in: U256, reserve_out: U256) -> U256 {
        if amount_in.is_zero() || reserve_in.is_zero() || reserve_out.is_zero() {
            return U256::ZERO;
        }
        let with_fee = amount_in * U256::from(10_000 - self.fee_bps as u64);
        with_fee * reserve_out / (reserve_in * U256::from(10_000u64) + with_fee)
    }
    fn orient(&self, r0: U256, r1: U256) -> (U256, U256) {
        if sort_tokens(self.token_in, self.token_out).0 == self.token_in {
            (r0, r1)
        } else {
            (r1, r0)
        }
    }
}

pub fn encode_create_task(p: &TaskParams) -> Bytes {
    createTaskCall { p: p.clone() }.abi_encode().into()
}

pub fn encode_approve(spender: Address, amount: U256) -> Bytes {
    approveCall { spender, amount }.abi_encode().into()
}

/// taskId as computed by the contract: keccak256(abi.encode(chainid, contract, owner, nonce)).
pub fn task_id(chain_id: u64, contract: Address, owner: Address, nonce: U256) -> B256 {
    keccak256((U256::from(chain_id), contract, owner, nonce).abi_encode())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{address, b256};

    pub const PANCAKE_FACTORY: Address = address!("cA143Ce32Fe78f1f7019d7d551a6402fC5350c73");
    pub const PANCAKE_INIT: B256 =
        b256!("00fb7f630766e6a796048ea87d01acd3068e8ff67d078148a3fa3f4a84f69bd5");
    const WBNB: Address = address!("bb4CdB9CBd36B01bD1cBaEBF2De08d9173bc095c");
    const SLERF: Address = address!("EbE3069bC7B5F1602De421f7a1089759F15AB636");

    fn slerf() -> PancakeV2 {
        PancakeV2 {
            factory: PANCAKE_FACTORY,
            init_code_hash: PANCAKE_INIT,
            token_in: SLERF,
            token_out: WBNB,
            fee_bps: 25,
        }
    }

    /// Real mainnet data: the SLERF/WBNB pair created in block 134305833.
    #[test]
    fn pair_address_matches_mainnet() {
        assert_eq!(
            slerf().pool(),
            address!("82b2c3f6DF175fC4644d1A27D72434a1322d26c9")
        );
    }

    /// Real mainnet data: first sale after the launch (tx 0x76b46526…, 500k tokens for 0.0447758402483806 BNB)
    /// against the initial reserves from the launch transaction's Sync event.
    #[test]
    fn quote_matches_mainnet_trade() {
        let a = slerf();
        let r_token = U256::from_str_radix("a56fa5b99019a5c8000000", 16).unwrap(); // 200,000,000e18
        let r_bnb = U256::from_str_radix("f9ccd8a1c5080000", 16).unwrap(); // 18e18
        let (rin, rout) = a.orient(r_bnb, r_token); // WBNB sorts first
        assert_eq!((rin, rout), (r_token, r_bnb));
        let out = a.quote(
            U256::from(500_000u64) * U256::from(10u64).pow(U256::from(18u64)),
            rin,
            rout,
        );
        assert_eq!(out, U256::from(44_775_840_248_380_600u64));
    }

    #[test]
    fn calldata_and_task_id() {
        let cd = slerf().sell_calldata(B256::repeat_byte(1), U256::from(5u64), U256::from(6u64));
        assert_eq!(&cd[..4], &executeCall::SELECTOR);
        assert_eq!(cd.len(), 4 + 96);
        // differs per chain, contract, owner and nonce
        let id = task_id(56, PANCAKE_FACTORY, WBNB, U256::ZERO);
        assert_ne!(id, task_id(97, PANCAKE_FACTORY, WBNB, U256::ZERO));
        assert_ne!(id, task_id(56, PANCAKE_FACTORY, WBNB, U256::from(1u64)));
        assert_ne!(id, task_id(56, WBNB, WBNB, U256::ZERO));
    }
}
