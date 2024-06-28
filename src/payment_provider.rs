use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use cln_rpc::{
    model::{
        requests::{
            ListpaysRequest, ListpaysStatus, ListpeerchannelsRequest, PayRequest,
            WaitsendpayRequest,
        },
        responses::PayStatus,
    },
    primitives::Amount,
};
#[cfg(test)]
use mockall::automock;
use secp256k1::hashes::sha256;
use tracing::{instrument, warn};

use crate::rpc::{ClnRpc, Rpc};

/// The `PaymentProvider` trait exposes a `pay` method.
#[cfg_attr(test, automock)]
#[async_trait]
pub trait PaymentProvider {
    /// `pay` pays the specified invoice. If a payment for this invoice is
    /// already in-flight, it returns when that payment is done. The return
    /// value is the preimage, if successful.
    async fn pay(&self, req: PaymentRequest) -> Result<Vec<u8>>;
    /// `wait_payment` waits until a payment is fully resolved and no htlcs for
    /// the given payment hash are outgoing anymore.
    async fn wait_payment(&self, payment_hash: sha256::Hash) -> Result<Option<Vec<u8>>>;
}

/// `PaymentProvider` using core lightning's `pay` method.
#[derive(Clone)]
pub struct PayPaymentProvider {
    rpc: Arc<Rpc>,
}

enum PaymentState {
    Pending,
    Free,
    Resolved(Vec<u8>),
}

impl PayPaymentProvider {
    /// Initializes a new `PayPaymentProvider`
    pub fn new(rpc: Arc<Rpc>) -> Self {
        Self { rpc }
    }

    /// Checks whether a payment is currently in-flight.
    async fn check_payment_done(&self, payment_hash: sha256::Hash) -> Result<PaymentState> {
        // If the payment is complete, ignore any htlc status, because any
        // trampoline payment can then be claimed from the client.
        if let Some(preimage) = self
            .rpc
            .listpays(&ListpaysRequest {
                payment_hash: Some(payment_hash),
                bolt11: None,
                status: Some(ListpaysStatus::COMPLETE),
            })
            .await?
            .pays
            .iter()
            .find_map(|pay| pay.preimage)
        {
            return Ok(PaymentState::Resolved(preimage.to_vec()));
        }

        // If there are pending payments, that means it's pending.
        if !self
            .rpc
            .listpays(&ListpaysRequest {
                payment_hash: Some(payment_hash),
                bolt11: None,
                status: Some(ListpaysStatus::PENDING),
            })
            .await?
            .pays
            .is_empty()
        {
            return Ok(PaymentState::Pending);
        }

        // If there are any htlcs still in the channel, it is still pending.
        if self
            .rpc
            .listpeerchannels(&ListpeerchannelsRequest { id: None })
            .await?
            .channels
            .map(|channels| {
                channels.into_iter().any(|channel| {
                    channel
                        .htlcs
                        .map(|htlcs| {
                            htlcs.iter().any(|htlc| {
                                htlc.payment_hash
                                    .map(|htlc_hash| htlc_hash == payment_hash)
                                    .unwrap_or(false)
                            })
                        })
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false)
        {
            return Ok(PaymentState::Pending);
        }

        Ok(PaymentState::Free)
    }
}

#[async_trait]
impl PaymentProvider for PayPaymentProvider {
    /// `pay` pays the specified invoice. If a payment for this invoice is
    /// already in-flight, it returns when that payment is done. The return
    /// value is the preimage, if successful.
    #[instrument(level = "trace", skip(self))]
    async fn pay(&self, req: PaymentRequest) -> Result<Vec<u8>> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let label = format!("trampoline-{}-{}", req.bolt11, now);

        match self.check_payment_done(req.payment_hash).await? {
            PaymentState::Pending => {
                return match self.wait_payment(req.payment_hash).await? {
                    Some(preimage) => Ok(preimage),
                    None => Err(anyhow!("payment failed")),
                };
            }
            PaymentState::Free => {}
            PaymentState::Resolved(preimage) => return Ok(preimage),
        };

        // TODO: extract the failure reason here?
        let resp = self
            .rpc
            .pay(&PayRequest {
                amount_msat: req.amount_msat.map(Amount::from_msat),
                bolt11: req.bolt11,
                label: Some(label),
                riskfactor: Some(20.0),
                maxfeepercent: None,
                retry_for: Some(30),
                maxdelay: Some(req.max_cltv_delta),
                exemptfee: None,
                localinvreqid: None,
                exclude: None,
                maxfee: Some(Amount::from_msat(req.max_fee_msat)),
                description: None,
            })
            .await?;

        match resp.status {
            // Note there is no need to check warning_partial_completion on
            // successful payments. If the payment partially completes, the
            // first thing to do is claim the payment from the sender, because
            // we've basically prepaid.
            PayStatus::COMPLETE => return Ok(resp.payment_preimage.to_vec()),
            PayStatus::PENDING => {
                warn!("payment is pending after pay returned");
                return match self.wait_payment(req.payment_hash).await? {
                    Some(preimage) => Ok(preimage),
                    None => Err(anyhow!("payment failed")),
                };
            }
            PayStatus::FAILED => {
                if let Some(warning) = resp.warning_partial_completion {
                    warn!("pay returned partial completion: {}", warning);
                    return match self.wait_payment(req.payment_hash).await? {
                        Some(preimage) => Ok(preimage),
                        None => Err(anyhow!("payment failed")),
                    };
                };
                return Err(anyhow!("payment failed"));
            }
        }
    }

    /// `wait_payment` waits until a payment is fully resolved and no htlcs for
    /// the given payment hash are outgoing anymore.
    async fn wait_payment(&self, payment_hash: sha256::Hash) -> Result<Option<Vec<u8>>> {
        if let Some(preimage) = match self
            .rpc
            .waitsendpay(&WaitsendpayRequest {
                payment_hash,
                timeout: None,
                partid: None,
                groupid: None,
            })
            .await
        {
            Ok(resp) => resp.payment_preimage.map(|p| p.to_vec()),
            Err(e) => match e {
                crate::rpc::RpcError::Rpc(rpc) => {
                    if let Some(code) = rpc.code {
                        match code {
                            200 => return Err(anyhow!("timeout")),
                            _ => None,
                        }
                    } else {
                        return Err(rpc.into());
                    }
                }
                crate::rpc::RpcError::General(e) => return Err(e),
            },
        } {
            return Ok(Some(preimage));
        };

        loop {
            match self.check_payment_done(payment_hash).await? {
                PaymentState::Pending => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
                PaymentState::Free => return Ok(None),
                PaymentState::Resolved(preimage) => return Ok(Some(preimage)),
            }
        }
    }
}

/// `PaymentRequest` defines the payment parameters.
#[derive(Clone, Debug, PartialEq)]
pub struct PaymentRequest {
    /// The bolt11 invoice to pay.
    pub bolt11: String,
    /// The payment hash of the invoice, for convenience.
    pub payment_hash: sha256::Hash,
    /// Should only be set if `bolt11` is a zero-amount invoice.
    pub amount_msat: Option<u64>,
    /// The maximum fee for the chosen route in millisatoshi.
    pub max_fee_msat: u64,
    /// The maximum delay in the chosen route in blocks.
    pub max_cltv_delta: u16,
}
