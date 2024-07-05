use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use cln_rpc::{
    model::{
        requests::{ListsendpaysRequest, ListsendpaysStatus, PayRequest, WaitsendpayRequest},
        responses::PayStatus,
    },
    primitives::Amount,
};
use futures::{stream::FuturesUnordered, StreamExt};
#[cfg(test)]
use mockall::automock;
use secp256k1::hashes::sha256;
use tokio::join;
use tracing::{debug, instrument, warn};

use crate::rpc::{ClnRpc, RpcError};

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
pub struct PayPaymentProvider<R>
where
    R: ClnRpc,
{
    rpc: Arc<R>,
}

impl<R> PayPaymentProvider<R>
where
    R: ClnRpc,
{
    /// Initializes a new `PayPaymentProvider`
    pub fn new(rpc: Arc<R>) -> Self {
        Self { rpc }
    }
}

#[async_trait]
impl<R> PaymentProvider for PayPaymentProvider<R>
where
    R: ClnRpc + Send + Sync,
{
    /// `pay` pays the specified invoice. If a payment for this invoice is
    /// already in-flight, it returns when that payment is done. The return
    /// value is the preimage, if successful.
    #[instrument(level = "trace", skip(self))]
    async fn pay(&self, req: PaymentRequest) -> Result<Vec<u8>> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let label = format!("trampoline-{}-{}", req.bolt11, now);

        // TODO: extract the failure reason here?
        let resp = match self
            .rpc
            .pay(&PayRequest {
                amount_msat: req.amount_msat.map(Amount::from_msat),
                partial_msat: None,
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
            .await
        {
            Ok(resp) => resp,
            Err(e) => {
                debug!("pay returned error {:?}", e);
                return match self.wait_payment(req.payment_hash).await? {
                    Some(preimage) => Ok(preimage),
                    None => Err(anyhow!("payment failed")),
                };
            }
        };

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
        let completed_req = ListsendpaysRequest {
            payment_hash: Some(payment_hash),
            bolt11: None,
            index: None,
            limit: None,
            start: None,
            status: Some(ListsendpaysStatus::COMPLETE),
        };
        let completed_payments_fut = self.rpc.listsendpays(&completed_req);
        let pending_req = ListsendpaysRequest {
            payment_hash: Some(payment_hash),
            bolt11: None,
            index: None,
            limit: None,
            start: None,
            status: Some(ListsendpaysStatus::PENDING),
        };
        let pending_payments_fut = self.rpc.listsendpays(&pending_req);
        let (completed_payments, pending_payments) =
            join!(completed_payments_fut, pending_payments_fut);
        let (completed_payments, pending_payments) = (completed_payments?, pending_payments?);

        if let Some(preimage) = completed_payments
            .payments
            .iter()
            .filter_map(|p| p.payment_preimage)
            .next()
        {
            return Ok(Some(preimage.to_vec()));
        }

        let mut tasks = FuturesUnordered::new();

        for payment in pending_payments.payments {
            tasks.push(self.rpc.waitsendpay(WaitsendpayRequest {
                groupid: Some(payment.groupid),
                partid: payment.partid,
                payment_hash,
                timeout: None,
            }));
        }

        while let Some(res) = tasks.next().await {
            match res {
                Ok(res) => {
                    if let Some(preimage) = res.payment_preimage {
                        return Ok(Some(preimage.to_vec()));
                    }
                }
                Err(e) => match e {
                    RpcError::Rpc(e) => match e.code {
                        Some(code) => match code {
                            -1 => return Err(anyhow!("{:?}", e)),
                            200 => return Err(anyhow!("timeout")),
                            202 => {}
                            203 => {}
                            204 => {}
                            208 => {}
                            209 => {}
                            _ => return Err(anyhow!("unknown rpc error code {}: {:?}", code, e)),
                        },
                        None => return Err(anyhow!("unknown rpc error without code {:?}", e)),
                    },
                    RpcError::General(e) => return Err(e),
                },
            }
        }

        Ok(None)
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use cln_rpc::{
        model::{
            requests::ListsendpaysStatus,
            responses::{
                ListsendpaysPayments, ListsendpaysPaymentsStatus, ListsendpaysResponse,
                WaitsendpayResponse, WaitsendpayStatus,
            },
        },
        primitives::{Amount, Secret},
    };
    use secp256k1::hashes::{sha256, Hash};
    use tracing_test::traced_test;

    use crate::rpc::MockClnRpc;

    use super::{PayPaymentProvider, PaymentProvider};

    fn preimage() -> Secret {
        [0; 32].to_vec().try_into().unwrap()
    }

    fn payment_hash() -> sha256::Hash {
        sha256::Hash::hash(&preimage().to_vec())
    }

    fn pending_payment() -> ListsendpaysPayments {
        ListsendpaysPayments {
            amount_msat: None,
            bolt11: None,
            bolt12: None,
            completed_at: None,
            created_index: None,
            description: None,
            destination: None,
            erroronion: None,
            label: None,
            partid: Some(1),
            payment_preimage: None,
            updated_index: None,
            status: ListsendpaysPaymentsStatus::PENDING,
            amount_sent_msat: Amount::from_sat(1),
            created_at: 0,
            groupid: 2,
            id: 0,
            payment_hash: payment_hash(),
        }
    }

    fn complete_payment() -> ListsendpaysPayments {
        ListsendpaysPayments {
            amount_msat: None,
            bolt11: None,
            bolt12: None,
            completed_at: None,
            created_index: None,
            description: None,
            destination: None,
            erroronion: None,
            label: None,
            partid: Some(1),
            payment_preimage: Some(preimage()),
            updated_index: None,
            status: ListsendpaysPaymentsStatus::COMPLETE,
            amount_sent_msat: Amount::from_sat(1),
            created_at: 0,
            groupid: 2,
            id: 0,
            payment_hash: payment_hash(),
        }
    }

    #[tokio::test]
    #[traced_test]
    async fn test_wait_payment_no_parts() {
        let mut rpc = MockClnRpc::new();
        rpc.expect_listsendpays()
            .withf(|req| {
                req.payment_hash == Some(payment_hash())
                    && req.status == Some(ListsendpaysStatus::COMPLETE)
            })
            .return_once(|_| Ok(ListsendpaysResponse { payments: vec![] }))
            .once();
        rpc.expect_listsendpays()
            .withf(|req| {
                req.payment_hash == Some(payment_hash())
                    && req.status == Some(ListsendpaysStatus::PENDING)
            })
            .return_once(|_| Ok(ListsendpaysResponse { payments: vec![] }))
            .once();
        let provider = PayPaymentProvider::new(Arc::new(rpc));

        let result = provider.wait_payment(payment_hash()).await;
        assert!(matches!(result, Ok(None)))
    }

    #[tokio::test]
    #[traced_test]
    async fn test_wait_payment_one_complete() {
        let mut rpc = MockClnRpc::new();
        rpc.expect_listsendpays()
            .withf(|req| {
                req.payment_hash == Some(payment_hash())
                    && req.status == Some(ListsendpaysStatus::COMPLETE)
            })
            .return_once(|_| {
                Ok(ListsendpaysResponse {
                    payments: vec![complete_payment()],
                })
            })
            .once();
        rpc.expect_listsendpays()
            .withf(|req| {
                req.payment_hash == Some(payment_hash())
                    && req.status == Some(ListsendpaysStatus::PENDING)
            })
            .return_once(|_| Ok(ListsendpaysResponse { payments: vec![] }))
            .once();
        let provider = PayPaymentProvider::new(Arc::new(rpc));

        let result = provider.wait_payment(payment_hash()).await;
        let preimage = preimage().to_vec();
        assert!(matches!(result, Ok(Some(p)) if p.eq(&preimage)))
    }

    #[tokio::test]
    #[traced_test]
    async fn test_wait_payment_one_pending() {
        let mut rpc = MockClnRpc::new();
        rpc.expect_listsendpays()
            .withf(|req| {
                req.payment_hash == Some(payment_hash())
                    && req.status == Some(ListsendpaysStatus::COMPLETE)
            })
            .return_once(|_| Ok(ListsendpaysResponse { payments: vec![] }))
            .once();
        rpc.expect_listsendpays()
            .withf(|req| {
                req.payment_hash == Some(payment_hash())
                    && req.status == Some(ListsendpaysStatus::PENDING)
            })
            .return_once(|_| {
                Ok(ListsendpaysResponse {
                    payments: vec![pending_payment()],
                })
            })
            .once();
        rpc.expect_waitsendpay()
            .withf(|p| {
                p.partid == Some(1) && p.groupid == Some(2) && p.payment_hash == payment_hash()
            })
            .return_once(|_| {
                Ok(WaitsendpayResponse {
                    amount_msat: None,
                    amount_sent_msat: Amount::from_sat(1),
                    bolt11: None,
                    bolt12: None,
                    label: None,
                    completed_at: None,
                    created_at: 0,
                    id: 0,
                    created_index: None,
                    groupid: None,
                    partid: None,
                    updated_index: None,
                    destination: None,
                    payment_hash: payment_hash(),
                    payment_preimage: Some(preimage()),
                    status: WaitsendpayStatus::COMPLETE,
                })
            })
            .once();
        let provider = PayPaymentProvider::new(Arc::new(rpc));

        let result = provider.wait_payment(payment_hash()).await;
        let preimage = preimage().to_vec();
        assert!(matches!(result, Ok(Some(p)) if p.eq(&preimage)))
    }

    #[tokio::test]
    #[traced_test]
    async fn test_wait_payment_one_pending_returns_code() {
        let mut rpc = MockClnRpc::new();
        rpc.expect_listsendpays()
            .withf(|req| {
                req.payment_hash == Some(payment_hash())
                    && req.status == Some(ListsendpaysStatus::COMPLETE)
            })
            .return_once(|_| Ok(ListsendpaysResponse { payments: vec![] }))
            .once();
        rpc.expect_listsendpays()
            .withf(|req| {
                req.payment_hash == Some(payment_hash())
                    && req.status == Some(ListsendpaysStatus::PENDING)
            })
            .return_once(|_| {
                Ok(ListsendpaysResponse {
                    payments: vec![pending_payment()],
                })
            })
            .once();
        rpc.expect_waitsendpay()
            .withf(|p| {
                p.partid == Some(1) && p.groupid == Some(2) && p.payment_hash == payment_hash()
            })
            .return_once(|_| {
                Err(crate::rpc::RpcError::Rpc(cln_rpc::RpcError {
                    code: Some(202),
                    data: None,
                    message: String::from(""),
                }))
            })
            .once();
        let provider = PayPaymentProvider::new(Arc::new(rpc));

        let result = provider.wait_payment(payment_hash()).await;
        assert!(matches!(result, Ok(None)))
    }
}
