use std::sync::Arc;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use cln_rpc::model::requests::{DatastoreMode, DatastoreRequest, ListdatastoreRequest};
use hex::ToHex;
use secp256k1::hashes::sha256;
use serde::{Deserialize, Serialize};
use tracing::instrument;

use crate::{
    messages::TrampolineInfo,
    rpc::{ClnRpc, Rpc},
};
#[cfg(test)]
use mockall::automock;

#[cfg_attr(test, automock)]
#[async_trait]
pub trait Datastore {
    async fn add_payment_attempt(&self, trampoline: &TrampolineInfo) -> Result<AttemptId>;
    async fn fetch_payment_info(&self, trampoline: &TrampolineInfo) -> Result<PaymentState>;
    async fn mark_failed(&self, trampoline: &TrampolineInfo, attempt_id: &AttemptId) -> Result<()>;
    async fn mark_succeeded(
        &self,
        trampoline: &TrampolineInfo,
        attempt_id: String,
        preimage: Vec<u8>,
    ) -> Result<()>;
}

#[derive(Serialize, Deserialize)]
struct AttemptInfo {
    amount_msat: u64,
    bolt11: String,
    completed: bool,
    success: bool,
}

pub struct ClnDatastore {
    rpc: Arc<Rpc>,
}

#[derive(Serialize, Deserialize)]
pub enum PaymentState {
    Free,
    Pending { attempt_id: String, attempt_time_seconds: u64 },
    Succeeded { preimage: Vec<u8> },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AttemptId {
    pub attempt_id: String,
    pub state_generation: u64,
}

impl ClnDatastore {
    pub fn new(rpc: Arc<Rpc>) -> Self {
        Self { rpc }
    }
}

#[async_trait]
impl Datastore for ClnDatastore {
    #[instrument(level = "trace", skip(self))]
    async fn add_payment_attempt(&self, trampoline: &TrampolineInfo) -> Result<AttemptId> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap();
        let attempt_id = now.as_nanos().to_string();
        let state = PaymentState::Pending {
            attempt_id: attempt_id.clone(),
            attempt_time_seconds: now.as_secs(),
        };
        let state = serde_json::to_string(&state)?;

        // TODO: double check whether we're not accidentally overwriting the preimage here?
        let state_generation = self
            .rpc
            .datastore(&DatastoreRequest {
                generation: None,
                key: state_key(trampoline.invoice.payment_hash()),
                string: Some(state),
                hex: None,
                mode: Some(DatastoreMode::CREATE_OR_REPLACE),
            })
            .await?
            .generation;

        let info = AttemptInfo {
            amount_msat: trampoline.amount_msat,
            bolt11: trampoline.bolt11.clone(),
            completed: false,
            success: false,
        };
        let info = serde_json::to_string(&info)?;
        self.rpc
            .datastore(&DatastoreRequest {
                generation: None,
                hex: None,
                key: attempt_key(trampoline.invoice.payment_hash(), attempt_id.clone()),
                mode: Some(DatastoreMode::MUST_CREATE),
                string: Some(info),
            })
            .await?;
        Ok(AttemptId {
            attempt_id,
            state_generation: state_generation.unwrap_or(0),
        })
    }

    #[instrument(level = "trace", skip(self))]
    async fn fetch_payment_info(&self, trampoline: &TrampolineInfo) -> Result<PaymentState> {
        Ok(
            match self
                .rpc
                .listdatastore(&ListdatastoreRequest {
                    key: Some(state_key(trampoline.invoice.payment_hash())),
                })
                .await?
                .datastore
                .into_iter()
                .nth(0)
            {
                Some(state) => {
                    serde_json::from_str(&state.string.ok_or(anyhow!("state missing"))?)?
                }
                None => PaymentState::Free,
            },
        )
    }

    #[instrument(level = "trace", skip(self))]
    async fn mark_failed(&self, trampoline: &TrampolineInfo, attempt_id: &AttemptId) -> Result<()> {
        let info = AttemptInfo {
            amount_msat: trampoline.amount_msat,
            bolt11: trampoline.bolt11.clone(),
            completed: true,
            success: false,
        };
        let info = serde_json::to_string(&info)?;
        self.rpc
            .datastore(&DatastoreRequest {
                key: attempt_key(
                    trampoline.invoice.payment_hash(),
                    attempt_id.attempt_id.clone(),
                ),
                string: Some(info),
                hex: None,
                mode: Some(DatastoreMode::MUST_REPLACE),
                generation: None,
            })
            .await?;
        let state = PaymentState::Free;
        let state = serde_json::to_string(&state)?;
        self.rpc
            .datastore(&DatastoreRequest {
                generation: Some(attempt_id.state_generation),
                string: Some(state),
                key: state_key(trampoline.invoice.payment_hash()),
                hex: None,
                mode: Some(DatastoreMode::MUST_REPLACE),
            })
            .await?;

        Ok(())
    }

    #[instrument(level = "trace", skip(self))]
    async fn mark_succeeded(
        &self,
        trampoline: &TrampolineInfo,
        attempt_id: String,
        preimage: Vec<u8>,
    ) -> Result<()> {
        let state = PaymentState::Succeeded { preimage };
        let state = serde_json::to_string(&state)?;
        self.rpc
            .datastore(&DatastoreRequest {
                key: state_key(trampoline.invoice.payment_hash()),
                string: Some(state),
                hex: None,
                mode: Some(DatastoreMode::CREATE_OR_REPLACE),
                generation: None,
            })
            .await?;
        let info = AttemptInfo {
            amount_msat: trampoline.amount_msat,
            bolt11: trampoline.bolt11.clone(),
            completed: true,
            success: true,
        };
        let info = serde_json::to_string(&info)?;
        self.rpc
            .datastore(&DatastoreRequest {
                key: attempt_key(trampoline.invoice.payment_hash(), attempt_id),
                string: Some(info),
                hex: None,
                mode: Some(DatastoreMode::MUST_REPLACE),
                generation: None,
            })
            .await?;
        Ok(())
    }
}

fn state_key(payment_hash: &sha256::Hash) -> Vec<String> {
    vec![
        String::from("trampoline"),
        String::from("payments"),
        payment_hash.encode_hex(),
        String::from("state"),
    ]
}

fn attempt_key(payment_hash: &sha256::Hash, attempt_id: String) -> Vec<String> {
    vec![
        String::from("trampoline"),
        String::from("payments"),
        payment_hash.encode_hex(),
        String::from("attempts"),
        attempt_id,
    ]
}
