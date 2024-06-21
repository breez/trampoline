use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use cln_rpc::{
    model::{requests::PayRequest, responses::PayStatus},
    primitives::Amount,
};
#[cfg(test)]
use mockall::automock;
use tracing::{instrument, warn};

use crate::rpc::Rpc;

/// The `PaymentProvider` trait exposes a `pay` method.
#[cfg_attr(test, automock)]
#[async_trait]
pub trait PaymentProvider {
    /// `pay` pays the specified invoice. If a payment for this invoice is
    /// already in-flight, it returns when that payment is done. The return
    /// value is the preimage, if successful.
    async fn pay(&self, req: PaymentRequest) -> Result<Vec<u8>>;
}

#[derive(Clone)]
pub struct PayPaymentProvider {
    rpc: Arc<Rpc>,
}

impl PayPaymentProvider {
    pub fn new(rpc: Arc<Rpc>) -> Self {
        Self { rpc }
    }
}

#[async_trait]
impl PaymentProvider for PayPaymentProvider {
    #[instrument(level = "trace", skip(self))]
    async fn pay(&self, req: PaymentRequest) -> Result<Vec<u8>> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let label = format!("trampoline-{}-{}", req.bolt11, now);

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
                // TODO: Ensure the payment is done before returning here.
                todo!()
            }
            PayStatus::FAILED => {
                if let Some(warning) = resp.warning_partial_completion {
                    warn!("pay returned partial completion: {}", warning);
                    // TODO: Ensure all parts fail (or one succeeds) before
                    // returning.
                    todo!()
                };
                return Err(anyhow!("payment failed"));
            }
        }
    }
}

/// `PaymentRequest` defines the payment parameters.
#[derive(Clone, Debug, PartialEq)]
pub struct PaymentRequest {
    /// The bolt11 invoice to pay.
    pub bolt11: String,
    /// Should only be set if `bolt11` is a zero-amount invoice.
    pub amount_msat: Option<u64>,
    /// The maximum fee for the chosen route in millisatoshi.
    pub max_fee_msat: u64,
    /// The maximum delay in the chosen route in blocks.
    pub max_cltv_delta: u16,
}
