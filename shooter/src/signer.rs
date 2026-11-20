//! Local signing for the dedicated executor account and nonce bookkeeping.
use alloy::consensus::{SignableTransaction, TxEnvelope, TxLegacy};
use alloy::eips::eip2718::Encodable2718;
use alloy::network::TxSignerSync;
use alloy::primitives::{Address, Bytes, TxKind, B256, U256};
use alloy::signers::local::PrivateKeySigner;
use anyhow::{bail, Context, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedTx {
    pub hash: B256,
    pub raw: Bytes,
    pub nonce: u64,
    pub gas_limit: u64,
    pub gas_price: u128,
    /// gas_limit × gas_price + value: the most this transaction can ever cost.
    pub max_cost: u128,
}

pub struct ExecSigner {
    inner: PrivateKeySigner,
    chain_id: u64,
}

impl std::fmt::Debug for ExecSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "ExecSigner({}, chain {})",
            self.inner.address(),
            self.chain_id
        )
    }
}

impl ExecSigner {
    /// Loads the key from a file (preferred) that must not be readable by group/others.
    pub fn from_file(path: &std::path::Path, chain_id: u64) -> Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(path)
                .with_context(|| format!("key file {}", path.display()))?
                .permissions()
                .mode();
            if mode & 0o077 != 0 {
                bail!(
                    "key file {} must have mode 0600 (found {:o})",
                    path.display(),
                    mode & 0o777
                );
            }
        }
        let s = std::fs::read_to_string(path)
            .with_context(|| format!("key file {}", path.display()))?;
        Self::from_secret(s.trim(), chain_id)
    }

    /// Loads the key from an environment variable. The value never appears in errors.
    pub fn from_env(var: &str, chain_id: u64) -> Result<Self> {
        let s = std::env::var(var)
            .map_err(|_| anyhow::anyhow!("environment variable {var} is not set"))?;
        Self::from_secret(s.trim(), chain_id)
    }

    pub fn from_secret(secret: &str, chain_id: u64) -> Result<Self> {
        // Do not attach the parse error: it could echo key material.
        let inner: PrivateKeySigner = secret
            .parse()
            .map_err(|_| anyhow::anyhow!("executor key is not a valid secp256k1 private key"))?;
        Ok(Self { inner, chain_id })
    }

    pub fn address(&self) -> Address {
        self.inner.address()
    }

    /// Legacy transactions keep the fee ceiling exact: cost ≤ gas_limit × gas_price.
    pub fn sign(
        &self,
        nonce: u64,
        gas_price: u128,
        gas_limit: u64,
        to: Address,
        value: U256,
        data: Bytes,
    ) -> Result<SignedTx> {
        self.sign_kind(nonce, gas_price, gas_limit, TxKind::Call(to), value, data)
    }

    /// Contract creation (used once, for deploying the executor contract).
    pub fn sign_create(
        &self,
        nonce: u64,
        gas_price: u128,
        gas_limit: u64,
        init_code: Bytes,
    ) -> Result<SignedTx> {
        self.sign_kind(
            nonce,
            gas_price,
            gas_limit,
            TxKind::Create,
            U256::ZERO,
            init_code,
        )
    }

    fn sign_kind(
        &self,
        nonce: u64,
        gas_price: u128,
        gas_limit: u64,
        to: TxKind,
        value: U256,
        data: Bytes,
    ) -> Result<SignedTx> {
        let mut tx = TxLegacy {
            chain_id: Some(self.chain_id),
            nonce,
            gas_price,
            gas_limit,
            to,
            value,
            input: data,
        };
        let sig = self
            .inner
            .sign_transaction_sync(&mut tx)
            .map_err(|_| anyhow::anyhow!("signing failed"))?;
        let env: TxEnvelope = tx.into_signed(sig).into();
        let value_wei: u128 = value
            .try_into()
            .map_err(|_| anyhow::anyhow!("value too large"))?;
        Ok(SignedTx {
            hash: *env.tx_hash(),
            raw: env.encoded_2718().into(),
            nonce,
            gas_limit,
            gas_price,
            max_cost: gas_limit as u128 * gas_price + value_wei,
        })
    }
}

/// Single owner of the executor account's nonce sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NonceManager {
    next: u64,
}

impl NonceManager {
    pub fn new(chain_pending_nonce: u64) -> Self {
        Self {
            next: chain_pending_nonce,
        }
    }
    pub fn peek(&self) -> u64 {
        self.next
    }
    pub fn take(&mut self) -> u64 {
        let n = self.next;
        self.next += 1;
        n
    }
    /// Never moves backwards below a nonce the chain already consumed; moves forward when the chain
    /// is ahead (e.g. a transaction was sent outside this process).
    pub fn reconcile(&mut self, chain_latest_nonce: u64, highest_in_flight: Option<u64>) {
        let floor = highest_in_flight
            .map(|n| n + 1)
            .unwrap_or(0)
            .max(chain_latest_nonce);
        self.next = floor;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const K: &str = "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d"; // anvil test key #1

    #[test]
    fn signs_deterministically_and_bounds_cost() {
        let s = ExecSigner::from_secret(K, 56).unwrap();
        let a = s
            .sign(
                3,
                50_000_000,
                300_000,
                Address::ZERO,
                U256::ZERO,
                Bytes::new(),
            )
            .unwrap();
        let b = s
            .sign(
                3,
                50_000_000,
                300_000,
                Address::ZERO,
                U256::ZERO,
                Bytes::new(),
            )
            .unwrap();
        assert_eq!(a, b);
        assert_eq!(a.max_cost, 15_000_000_000_000);
        assert_ne!(
            a.hash,
            s.sign(
                4,
                50_000_000,
                300_000,
                Address::ZERO,
                U256::ZERO,
                Bytes::new()
            )
            .unwrap()
            .hash
        );
    }

    #[test]
    fn secrets_never_leak_into_debug_or_errors() {
        let s = ExecSigner::from_secret(K, 56).unwrap();
        assert!(!format!("{s:?}").contains("59c6995e"));
        let bad = "0xdeadbeefnotakey";
        let e = ExecSigner::from_secret(bad, 56).unwrap_err();
        assert!(!format!("{e:#}").contains("deadbeef"));
    }

    #[cfg(unix)]
    #[test]
    fn key_file_must_be_private() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("key");
        std::fs::write(&p, K).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(ExecSigner::from_file(&p, 56).is_err());
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(ExecSigner::from_file(&p, 56).is_ok());
    }

    #[test]
    fn nonce_reconcile() {
        let mut n = NonceManager::new(5);
        assert_eq!((n.take(), n.take()), (5, 6));
        n.reconcile(6, Some(6)); // 5 mined, 6 still in flight
        assert_eq!(n.peek(), 7);
        n.reconcile(9, None); // someone else used the account
        assert_eq!(n.peek(), 9);
    }
}
