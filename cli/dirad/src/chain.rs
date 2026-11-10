//! The per-device batch hash chain of contract 2.0 (DIRASH-0052).
//!
//! Every attestation batch carries `seq`, `prevBatchHash` and `chainEpoch`.
//! `seq` and `prevBatchHash` advance only when a batch is **acknowledged**, so
//! a retried batch is re-sent byte-identical (same `batchId`, same position)
//! and the cloud deduplicates it. A chunk the device steps past (quarantine)
//! never advances the chain: the cloud never accepted it.
//!
//! State lives in `meta` (`batch_seq` = the next seq, `batch_prev_hash`,
//! `chain_epoch`). A local wipe (`Store::nuke`) and a cloud data-epoch reset
//! start a new epoch at seq 0.

use dira_contract::{AttestationBatch, ClientInfo};
use dira_core::sync::{META_BATCH_PREV_HASH, META_BATCH_SEQ, META_CHAIN_EPOCH};
use dira_core::Store;
use sha2::{Digest, Sha256};

/// Where the chain stands: the position the next batch takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chain {
    pub seq: u64,
    pub prev_hash: String,
    pub epoch: u32,
}

impl Chain {
    /// Read the chain from `meta`. Missing or blank keys are the start of
    /// epoch 0 (or of the stored epoch).
    pub async fn load(store: &Store) -> Self {
        let get = |k: &'static str| async move {
            store
                .meta_get(k)
                .await
                .ok()
                .flatten()
                .filter(|v| !v.is_empty())
        };
        Self {
            seq: get(META_BATCH_SEQ)
                .await
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
            prev_hash: get(META_BATCH_PREV_HASH).await.unwrap_or_default(),
            epoch: get(META_CHAIN_EPOCH)
                .await
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
        }
    }

    /// Put this position, and the producing client, on `batch`. Must run
    /// before signing: all of it is part of the signed payload.
    pub fn stamp(&self, batch: &mut AttestationBatch) {
        batch.seq = self.seq;
        batch.prev_batch_hash = self.prev_hash.clone();
        batch.chain_epoch = self.epoch;
        batch.client = Some(client_info());
    }

    /// Record that the batch hashing to `hash` was acknowledged. The seq and
    /// the hash are written in one transaction: a crash between two writes
    /// would leave seq+1 with the old hash, and the next batch would read as a
    /// fork.
    pub async fn advance(&mut self, store: &Store, hash: String) -> Result<(), String> {
        let seq = (self.seq + 1).to_string();
        store
            .meta_set_many(&[(META_BATCH_SEQ, &seq), (META_BATCH_PREV_HASH, &hash)])
            .await
            .map_err(|e| format!("advance chain: {e}"))?;
        self.seq += 1;
        self.prev_hash = hash;
        Ok(())
    }

    /// Start a new epoch at seq 0 (the cloud reset its data epoch).
    pub async fn restart(store: &Store) -> Result<(), String> {
        let epoch = Self::load(store).await.epoch + 1;
        store
            .meta_set_many(&[
                (META_CHAIN_EPOCH, &epoch.to_string()),
                (META_BATCH_SEQ, ""),
                (META_BATCH_PREV_HASH, ""),
            ])
            .await
            .map_err(|e| format!("restart chain: {e}"))
    }
}

/// Lowercase hex SHA-256 of the batch's RFC 8785 encoding: the
/// `prevBatchHash` the next batch carries.
pub fn hash(batch: &AttestationBatch) -> Result<String, String> {
    let bytes = dira_core::signing::canonical_bytes(batch).map_err(|e| e.to_string())?;
    Ok(hash_canonical(&bytes))
}

/// [`hash`] over bytes already canonicalized for signing, so a sender encodes
/// once and hashes and signs the same bytes.
pub fn hash_canonical(canonical: &[u8]) -> String {
    hex::encode(Sha256::digest(canonical))
}

/// This build and platform, read once per process. The channel comes from the
/// config at the first batch; a later `update.channel` change shows from the
/// next daemon start.
pub fn client_info() -> ClientInfo {
    static CLIENT: std::sync::OnceLock<ClientInfo> = std::sync::OnceLock::new();
    CLIENT
        .get_or_init(|| {
            let channel = dira_core::Config::load()
                .ok()
                .and_then(|c| c.update.channel)
                .or_else(|| {
                    let pre = env!("CARGO_PKG_VERSION").contains('-');
                    Some(if pre { "prerelease" } else { "stable" }.to_string())
                });
            ClientInfo {
                version: env!("CARGO_PKG_VERSION").to_string(),
                channel,
                os: std::env::consts::OS.to_string(),
                arch: std::env::consts::ARCH.to_string(),
            }
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn batch() -> AttestationBatch {
        AttestationBatch {
            batch_id: "b".into(),
            device_id: "d".into(),
            generated_at: "t".into(),
            seq: 0,
            prev_batch_hash: String::new(),
            chain_epoch: 0,
            client: None,
            capture_health: None,
            intervals: vec![],
            sessions: vec![],
            token_usage: vec![],
            artifacts: vec![],
            day_rollups: vec![],
            meetings: vec![],
            windows: vec![],
        }
    }

    #[tokio::test]
    async fn advances_only_on_ack_and_restarts_an_epoch() {
        let store = Store::open_in_memory().await.unwrap();
        let mut chain = Chain::load(&store).await;
        assert_eq!(
            (chain.seq, chain.epoch, chain.prev_hash.as_str()),
            (0, 0, "")
        );

        let mut b = batch();
        chain.stamp(&mut b);
        let h = hash(&b).unwrap();
        assert_eq!(h.len(), 64);
        chain.advance(&store, h.clone()).await.unwrap();

        let reloaded = Chain::load(&store).await;
        assert_eq!((reloaded.seq, reloaded.prev_hash.clone()), (1, h));

        let mut next = batch();
        reloaded.stamp(&mut next);
        assert_eq!(next.seq, 1);
        assert_eq!(next.prev_batch_hash.len(), 64);
        assert!(next.client.is_some());

        Chain::restart(&store).await.unwrap();
        let fresh = Chain::load(&store).await;
        assert_eq!(
            (fresh.seq, fresh.epoch, fresh.prev_hash.as_str()),
            (0, 1, "")
        );
    }

    #[test]
    fn the_hash_covers_the_chain_position() {
        let mut a = batch();
        let mut b = batch();
        b.seq = 1;
        a.prev_batch_hash = String::new();
        assert_ne!(hash(&a).unwrap(), hash(&b).unwrap());
    }
}
