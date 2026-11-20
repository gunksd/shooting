use alloy::primitives::{Address, Bytes, FixedBytes, B256, U256};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Nothing is broadcast while this is true. Defaults to true.
    #[serde(default = "yes")]
    pub dry_run: bool,
    pub chain_id: u64,
    pub db_path: PathBuf,
    pub rpc: RpcConfig,
    pub executor: ExecutorConfig,
    pub budget: BudgetConfig,
    #[serde(default)]
    pub channels: Vec<ChannelConfig>,
    #[serde(default)]
    pub tasks: Vec<TaskSpec>,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RpcConfig {
    pub http: Vec<String>,
    #[serde(default)]
    pub ws: Vec<String>,
    /// WebSocket endpoints that support `newPendingTransactions` with full objects. Optional.
    #[serde(default)]
    pub pending_ws: Vec<String>,
    #[serde(default = "d_timeout")]
    pub timeout_ms: u64,
    /// How far back logs are re-read after a reconnect or restart.
    #[serde(default = "d_backfill")]
    pub backfill_blocks: u64,
}
fn d_timeout() -> u64 {
    1500
}
fn d_backfill() -> u64 {
    64
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutorConfig {
    pub key_file: Option<PathBuf>,
    /// Name of an environment variable holding the key. The key itself never goes in this file.
    pub key_env: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetConfig {
    /// Total execution budget per task in micro-USDT. 500000 = 0.5 USDT.
    #[serde(default = "d_usd")]
    pub usd_micros: u64,
    #[serde(default = "d_age")]
    pub price_max_age_s: u64,
    pub min_price_micros: u64,
    pub max_price_micros: u64,
    #[serde(default = "d_buf")]
    pub buffer_bps: u16,
    /// V2 pair used as the BNB/USDT price source and the stable token in it.
    pub price_pair: Address,
    pub price_stable: Address,
    /// Count recorded initialization costs (deploy/approve/createTask) against the task budget.
    #[serde(default)]
    pub include_init_costs: bool,
}
fn d_usd() -> u64 {
    500_000
}
fn d_age() -> u64 {
    30
}
fn d_buf() -> u16 {
    500
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChannelKind {
    Public,
    Club48,
    Blockrazor,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelConfig {
    pub name: String,
    pub kind: ChannelKind,
    pub url: String,
    /// Environment variable holding the Authorization header value, if the channel needs one.
    pub auth_env: Option<String>,
    #[serde(default = "d_min_interval")]
    pub min_interval_ms: u64,
}
fn d_min_interval() -> u64 {
    0
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Only fires as a bundle behind a matched pending target. Never falls back.
    StrictSameBlock,
    /// Fires once the launch is seen in a block, aiming for the block after.
    NextBlockBestEffort,
    /// Sends budget-capped attempts every block while armed; early ones revert cheaply.
    PreArmedSpray,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PendingRule {
    pub to: Address,
    pub selector: FixedBytes<4>,
    pub from: Option<Address>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Probe {
    /// `eth_call` whose 32-byte result is compared with `min_value`; armed when result >= min_value.
    pub to: Address,
    pub data: Bytes,
    pub min_value: U256,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TaskSpec {
    pub task_id: B256,
    pub executor_contract: Address,
    pub owner: Address,
    pub token_in: Address,
    pub token_out: Address,
    pub factory: Address,
    pub pair_init_code_hash: B256,
    #[serde(default = "d_fee")]
    pub pair_fee_bps: u16,
    pub amount_in: U256,
    /// Executor-side minimum; the contract rejects anything below the owner's on-chain floor.
    pub min_out: U256,
    pub mode: Mode,
    pub gas_limit: u64,
    pub gas_price_wei: u128,
    pub channels: Vec<String>,
    /// Sending the same transaction publicly and privately is an explicit choice.
    #[serde(default)]
    pub allow_public_with_private: bool,
    /// Total transactions this task may broadcast (all modes).
    #[serde(default = "d_one")]
    pub max_attempts: u32,
    /// Blocks after the trigger/arming during which the task may still act.
    #[serde(default = "d_window")]
    pub window_blocks: u64,
    /// Require a successful `eth_call` of the sale before firing (catches "pool exists, trading closed").
    #[serde(default = "yes")]
    pub require_sim_ok: bool,
    #[serde(default)]
    pub spray_per_block: u32,
    pub arm_probe: Option<Probe>,
    #[serde(default)]
    pub strict_targets: Vec<PendingRule>,
}
fn d_fee() -> u16 {
    25
}
fn d_one() -> u32 {
    1
}
fn d_window() -> u64 {
    3
}

impl Config {
    pub fn load(path: &std::path::Path) -> Result<Self> {
        let s =
            std::fs::read_to_string(path).with_context(|| format!("config {}", path.display()))?;
        let c: Config = toml::from_str(&s).context("config parse")?;
        c.validate()?;
        Ok(c)
    }

    pub fn validate(&self) -> Result<()> {
        if self.rpc.http.is_empty() {
            bail!("rpc.http needs at least one endpoint");
        }
        if self.executor.key_file.is_some() == self.executor.key_env.is_some() {
            bail!("set exactly one of executor.key_file / executor.key_env");
        }
        for t in &self.tasks {
            t.validate(&self.channels)?;
        }
        Ok(())
    }
}

impl TaskSpec {
    pub fn validate(&self, channels: &[ChannelConfig]) -> Result<()> {
        if self.amount_in.is_zero() || self.min_out.is_zero() {
            bail!(
                "task {}: amount_in and min_out must be non-zero (min_out = 0 is never allowed)",
                self.task_id
            );
        }
        if self.channels.is_empty()
            || self.max_attempts == 0
            || self.gas_limit < 21_000
            || self.gas_price_wei == 0
        {
            bail!(
                "task {}: channels, max_attempts, gas_limit and gas_price_wei are required",
                self.task_id
            );
        }
        let mut kinds = vec![];
        for name in &self.channels {
            let c = channels
                .iter()
                .find(|c| &c.name == name)
                .with_context(|| format!("task {}: unknown channel {name}", self.task_id))?;
            kinds.push(c.kind);
        }
        let has_public = kinds.contains(&ChannelKind::Public);
        let has_private = kinds.iter().any(|k| *k != ChannelKind::Public);
        if has_public && has_private && !self.allow_public_with_private {
            bail!("task {}: mixing public and private channels requires allow_public_with_private = true", self.task_id);
        }
        match self.mode {
            Mode::StrictSameBlock => {
                if self.strict_targets.is_empty() {
                    bail!(
                        "task {}: strict_same_block needs strict_targets",
                        self.task_id
                    );
                }
                if has_public {
                    bail!(
                        "task {}: strict_same_block cannot use a public channel",
                        self.task_id
                    );
                }
            }
            Mode::PreArmedSpray => {
                if self.spray_per_block == 0 {
                    bail!(
                        "task {}: pre_armed_spray needs spray_per_block >= 1",
                        self.task_id
                    );
                }
            }
            Mode::NextBlockBestEffort => {}
        }
        Ok(())
    }
}
