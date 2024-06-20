use std::{collections::HashMap, str::from_utf8, sync::Arc, time::Duration};

use lightning_invoice::Bolt11Invoice;
use secp256k1::PublicKey;
use tokio::sync::{mpsc, oneshot, Mutex};

use anyhow::{anyhow, Result};
use secp256k1::hashes::sha256::Hash;
use tracing::{debug, error, field, instrument, trace, warn};

use crate::{
    messages::{HtlcAcceptedRequest, HtlcAcceptedResponse, TrampolineRoutingPolicy},
    payment_provider::{PaymentProvider, PaymentRequest},
};

pub struct HtlcManager<P>
where
    P: PaymentProvider,
{
    local_pubkey: PublicKey,
    routing_policy: TrampolineRoutingPolicy,
    mpp_timeout: Duration,
    payments: Arc<Mutex<HashMap<Hash, PaymentState>>>,
    payment_provider: Arc<P>,
}

impl<P> HtlcManager<P>
where
    P: PaymentProvider + Send + Sync + 'static,
{
    pub fn new(
        local_pubkey: PublicKey,
        routing_policy: TrampolineRoutingPolicy,
        mpp_timeout: Duration,
        payment_provider: Arc<P>,
    ) -> Self {
        Self {
            local_pubkey,
            routing_policy,
            mpp_timeout,
            payments: Arc::new(Mutex::new(HashMap::new())),
            payment_provider,
        }
    }

    #[instrument(
        skip(self),
        fields(
            payment_hash = hex::encode(&req.htlc.payment_hash),
            short_channel_id = %req.htlc.short_channel_id,
            htlc_id = req.htlc.id))]
    pub async fn handle_htlc(&self, req: &HtlcAcceptedRequest) -> HtlcAcceptedResponse {
        let (sender, receiver) = oneshot::channel();
        let trampoline = match self.check_htlc(req) {
            HtlcCheckResult::Response(resp) => return resp,
            HtlcCheckResult::Trampoline(trampoline) => trampoline,
        };

        {
            let mut payments = self.payments.lock().await;
            let payment_state = payments
                .entry(*trampoline.invoice.payment_hash())
                .or_insert_with(|| {
                    let (s1, r1) = mpsc::channel(1);
                    let (s2, r2) = mpsc::channel(1);
                    // TODO: What to do with this watch handle?
                    tokio::spawn(watch_payment(
                        Arc::clone(&self.payment_provider),
                        Arc::clone(&self.payments),
                        *trampoline.clone(),
                        self.mpp_timeout,
                        r1,
                        r2,
                    ));
                    PaymentState::new(*trampoline.clone(), s1, s2)
                });

            // If the trampoline info doesn't match previous trampoline infos,
            // fail the payment asap.
            if *trampoline != payment_state.trampoline {
                payment_state
                    .fail(self.temporary_trampoline_failure())
                    .await;
            }

            // TODO: Deduplicate replayed htlcs?

            // Do add the htlc to the payment state always, also if it has
            // failed. It could be a payment was already in-flight, so 
            // eventually this htlc settles.
            payment_state.add_htlc(req, sender).await;
        }

        receiver.await.unwrap()
    }

    fn check_htlc(&self, req: &HtlcAcceptedRequest) -> HtlcCheckResult {
        // For trampoline payments we appear to be the destination. If we're not the
        // destination, continue.
        if req.onion.short_channel_id.is_some() {
            trace!("This is a forward, returning continue.");
            return HtlcCheckResult::Response(default_response());
        }

        // Check whether this is a trampoline payment by extracting the invoice.
        let trampoline = match self.extract_trampoline_info(req) {
            Ok(trampoline) => match trampoline {
                Some(trampoline) => trampoline,
                None => {
                    trace!("This is a not a trampoline htlc, returning continue.");
                    return HtlcCheckResult::Response(default_response());
                }
            },
            Err(e) => {
                debug!("Failed to extract trampoline info from htlc: {:?}", e);
                return HtlcCheckResult::Response(default_response());
            }
        };

        // If we are the destination, let the invoice subsystem handle this
        // htlc.
        // TODO: Double check whether this makes sense.
        if trampoline.payee.eq(&self.local_pubkey) {
            trace!("We are the payee, invoice subsystem handles this htlc.");
            return HtlcCheckResult::Response(default_response());
        }

        // The total is either set in the onion, or it's the forward amount of
        // the current htlc.
        let total_msat = match req.onion.total_msat {
            Some(total_msat) => total_msat,
            None => req.onion.forward_msat,
        };

        // Ensure enough fees are paid according to the routing policy.
        if !self
            .routing_policy
            .fee_sufficient(total_msat, trampoline.amount_msat)
        {
            trace!(
                total_msat = total_msat, 
                trampoline.amount_msat = trampoline.amount_msat,
                policy = field::debug(&self.routing_policy),
                "Payment offers too low fee for trampoline.");
            return HtlcCheckResult::Response(self.temporary_trampoline_failure());
        }

        // Ensure there's enough relative time to claim htlcs.
        if req.htlc.cltv_expiry_relative < self.routing_policy.cltv_expiry_delta as i64 {
            trace!(
                cltv_expiry_relative = req.htlc.cltv_expiry_relative,
                policy_cltv_expiry_delta = self.routing_policy.cltv_expiry_delta,
                "Relative cltv expiry too low.");
            return HtlcCheckResult::Response(self.temporary_trampoline_failure());
        }

        // Check whether we are the last hop in the route hint. We can't rewrite
        // this as a forward, so we'll error if that's the case.
        let route_hints = trampoline.invoice.route_hints();
        if let Some(our_hint) = route_hints.iter().find(|hint| {
            hint.0
                .last()
                .map(|hop| hop.src_node_id.eq(&self.local_pubkey))
                .unwrap_or(false)
        }) {
            let _ = match our_hint.0.last() {
                Some(our_hop) => our_hop,
                None => {
                    error!(
                        "Unpacked hint with hop, but didn't contain hop: {:?}",
                        our_hint
                    );
                    return HtlcCheckResult::Response(
                        HtlcAcceptedResponse::temporary_node_failure(),
                    );
                }
            };

            debug!(
                "Got invoice with ourselves in the hint. Erroring because this is not supported."
            );
            return HtlcCheckResult::Response(HtlcAcceptedResponse::temporary_node_failure());
        }

        debug!("This is a trampoline payment.");
        HtlcCheckResult::Trampoline(Box::new(trampoline))
    }

    fn extract_trampoline_info(&self, req: &HtlcAcceptedRequest) -> Result<Option<TrampolineInfo>> {
        let invoice_blob = match req.onion.payload.get(33001) {
            Some(invoice_blob) => invoice_blob.value,
            None => return Ok(None),
        };

        let invoice_str = match from_utf8(&invoice_blob) {
            Ok(invoice_str) => invoice_str,
            Err(e) => {
                debug!("Got invalid trampoline invoice in htlc, not utf-8: {:?}", e);
                return Err(anyhow!("invalid trampoline invoice in tlv"));
            }
        };

        let invoice: Bolt11Invoice = match invoice_str.parse() {
            Ok(invoice) => invoice,
            Err(e) => {
                debug!(
                    "Got invalid trampoline invoice in htlc, not an invoice: {:?}",
                    e
                );
                return Err(anyhow!("invalid trampoline invoice in tlv"));
            }
        };

        // For now invoices need to have a valid signature, because the `pay`
        // command requires invoices to have a valid signature. Once we move away
        // from the `pay` command, we can remove this check. (note that when
        // removing this the payee pubkey check below needs to be rechecked so it
        // never panics)
        if invoice.check_signature().is_err() {
            return Err(anyhow!("invalid signature in trampoline invoice"));
        }

        // Note that this may panic if the signature is not checked.
        let payee = invoice.get_payee_pub_key();

        // Extract optional amount from the TLV
        let tlv_amount_msat = match req.onion.payload.get(33003) {
            Some(amount_blob) => {
                if amount_blob.value.len() != 8 {
                    debug!(
                        "Got invalid amount of len {} in htlc TLV",
                        amount_blob.value.len()
                    );
                    None
                } else {
                    Some(u64::from_be_bytes(amount_blob.value.try_into().unwrap()))
                }
            }
            None => None,
        };

        // Either the invoice has an amount or the amount is set in the TLV.
        let amount_msat = match invoice.amount_milli_satoshis() {
            Some(invoice_amount_msat) => match tlv_amount_msat {
                Some(tlv_amount_msat) => {
                    if invoice_amount_msat == tlv_amount_msat {
                        invoice_amount_msat
                    } else {
                        return Err(anyhow!(
                            "non-matching amounts in invoice tlv and amount tlv"
                        ));
                    }
                }
                None => invoice_amount_msat,
            },
            None => match tlv_amount_msat {
                Some(amount_msat) => amount_msat,
                None => return Err(anyhow!("missing amount in invoice and amount tlv")),
            },
        };

        Ok(Some(TrampolineInfo {
            routing_policy: self.routing_policy.clone(),
            amount_msat,
            bolt11: String::from(invoice_str),
            invoice,
            payee,
        }))
    }

    fn temporary_trampoline_failure(&self) -> HtlcAcceptedResponse {
        HtlcAcceptedResponse::temporary_trampoline_failure(self.routing_policy.clone())
    }
}

fn default_response() -> HtlcAcceptedResponse {
    HtlcAcceptedResponse::Continue { payload: None }
}

#[instrument(
    skip(payment_provider, payments, payment_ready, fail_requested),
    fields(
        payment_hash = %trampoline.invoice.payment_hash(),
        bolt11 = trampoline.bolt11))]
async fn watch_payment<P>(
    payment_provider: Arc<P>,
    payments: Arc<Mutex<HashMap<Hash, PaymentState>>>,
    trampoline: TrampolineInfo,
    mpp_timeout: Duration,
    mut payment_ready: mpsc::Receiver<()>,
    mut fail_requested: mpsc::Receiver<HtlcAcceptedResponse>,
) where
    P: PaymentProvider,
{
    // TODO: Check whether we already have the preimage.

    tokio::select! {
        _ = tokio::time::sleep(mpp_timeout) => {
            debug!("Payment timed out waiting for htlcs.");
            let mut payments = payments.lock().await;
            let mut state = payments.remove(trampoline.invoice.payment_hash())
                .expect("Payment timed out waiting for htlcs, but payment was already gone.");
            state.resolve(HtlcAcceptedResponse::temporary_trampoline_failure(
                trampoline.routing_policy.clone()));

            return;
        }
        failure = fail_requested.recv() => {
            let failure = match failure {
                Some(failure) => failure,
                None => {
                    error!("fail_requested receiver was closed when it shouldn't be");
                    HtlcAcceptedResponse::temporary_node_failure()
                },
            };

            debug!("Payment fail requested.");
            let mut payments = payments.lock().await;
            let mut state = payments.remove(trampoline.invoice.payment_hash())
                .expect("Fail requested for payment, but payment was already gone");
            state.resolve(failure);

            return;
        }
        _ = payment_ready.recv() => {
            debug!("Received payment ready.");
        }
    };

    let amount_msat = match trampoline.invoice.amount_milli_satoshis() {
        Some(_) => None,
        None => Some(trampoline.amount_msat),
    };

    let max_fee_msat = {
        let payments = payments.lock().await;
        let payment = payments
            .get(trampoline.invoice.payment_hash())
            .expect("Payment is ready for paying, but payment was already gone.");
        payment
            .amount_received_msat
            .saturating_sub(trampoline.amount_msat)
    };

    // TODO: re-check the cltv expiry.
    // TODO: Persist we're about to pay.
    trace!("about to pay.");
    let pay_result = payment_provider
        .pay(PaymentRequest {
            bolt11: trampoline.bolt11,
            amount_msat,
            max_fee_msat,
        })
        .await;

    let mut payments = payments.lock().await;
    let mut payment = payments
        .remove(trampoline.invoice.payment_hash())
        .expect("Payment just returned from paying, but payment was already gone.");

    match pay_result {
        Ok(preimage) => {
            debug!("Payment succeeded, resolving payment with preimage.");
            // TODO: Persist the preimage
            payment.resolve(HtlcAcceptedResponse::Resolve {
                payment_key: preimage,
            });
        }
        Err(e) => {
            debug!(error = field::debug(e), "Payment failed.");
            // TODO: Extract relevant info from the error. And persist failure.
            payment.resolve(HtlcAcceptedResponse::temporary_trampoline_failure(
                trampoline.routing_policy.clone(),
            ));
        }
    }
}

struct PaymentState {
    htlcs: Vec<oneshot::Sender<HtlcAcceptedResponse>>,
    trampoline: TrampolineInfo,
    is_ready: bool,
    payment_ready: mpsc::Sender<()>,
    fail_requested: mpsc::Sender<HtlcAcceptedResponse>,
    amount_received_msat: u64,
    cltv_expiry: u32,
    resolution: Option<HtlcAcceptedResponse>,
}

impl PaymentState {
    fn new(
        trampoline: TrampolineInfo,
        payment_ready: mpsc::Sender<()>,
        fail_requested: mpsc::Sender<HtlcAcceptedResponse>,
    ) -> Self {
        Self {
            htlcs: Vec::new(),
            trampoline,
            is_ready: false,
            payment_ready,
            fail_requested,
            amount_received_msat: 0,
            cltv_expiry: u32::MAX,
            resolution: None,
        }
    }

    async fn add_htlc(
        &mut self,
        req: &HtlcAcceptedRequest,
        sender: oneshot::Sender<HtlcAcceptedResponse>,
    ) {
        if let Some(resolution) = &self.resolution {
            let _ = sender.send(resolution.clone());
            return;
        }

        self.amount_received_msat += req.htlc.amount_msat;
        self.cltv_expiry = std::cmp::min(req.htlc.cltv_expiry, self.cltv_expiry);
        self.htlcs.push(sender);
        if !self.is_ready
            && self
                .trampoline
                .routing_policy
                .fee_sufficient(self.amount_received_msat, self.trampoline.amount_msat)
        {
            trace!(
                amount_received_msat = self.amount_received_msat,
                trampoline.amount_msat = self.trampoline.amount_msat,
                "Payment is ready."
            );
            self.is_ready = true;
            let _ = self.payment_ready.send(()).await;
        }
    }

    async fn fail(&self, resp: HtlcAcceptedResponse) {
        let _ = self.fail_requested.send(resp).await;
    }

    fn resolve(&mut self, resp: HtlcAcceptedResponse) {
        self.resolution = Some(resp.clone());

        while let Some(listener) = self.htlcs.pop() {
            match listener.send(resp.clone()) {
                Ok(_) => {}
                Err(e) => warn!("htlc listener hung up, could not send response {:?}", e),
            };
        }
    }
}

enum HtlcCheckResult {
    Response(HtlcAcceptedResponse),
    Trampoline(Box<TrampolineInfo>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TrampolineInfo {
    pub bolt11: String,
    pub invoice: Bolt11Invoice,
    pub payee: PublicKey,
    pub amount_msat: u64,
    pub routing_policy: TrampolineRoutingPolicy,
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use anyhow::anyhow;
    use lightning_invoice::{Currency, InvoiceBuilder, PaymentSecret};
    use mockall::predicate::eq;
    use secp256k1::{
        hashes::{sha256, Hash},
        PublicKey, Secp256k1, SecretKey,
    };

    use crate::{
        htlc_manager::HtlcManager,
        messages::{
            Htlc, HtlcAcceptedRequest, HtlcAcceptedResponse, HtlcFailReason, Onion,
            TrampolineRoutingPolicy,
        },
        payment_provider::{MockPaymentProvider, PaymentRequest},
        tlv::{SerializedTlvStream, TlvEntry},
    };

    fn htlc_manager(payment_provider: MockPaymentProvider) -> HtlcManager<MockPaymentProvider> {
        HtlcManager::new(
            local_pubkey(),
            policy(),
            mpp_timeout(),
            Arc::new(payment_provider),
        )
    }

    fn invoice_amount() -> u64 {
        1_000_000
    }

    fn invoice_bytes() -> Vec<u8> {
        invoice_string().as_bytes().to_vec()
    }

    fn invoice_string() -> String {
        let private_key = SecretKey::from_slice(
            &[
                0xe1, 0x26, 0xf6, 0x8f, 0x7e, 0xaf, 0xcc, 0x8b, 0x74, 0xf5, 0x4d, 0x26, 0x9f, 0xe2,
                0x06, 0xbe, 0x71, 0x50, 0x00, 0xf9, 0x4d, 0xac, 0x06, 0x7d, 0x1c, 0x04, 0xa8, 0xca,
                0x3b, 0x2d, 0xb7, 0x35,
            ][..],
        )
        .unwrap();

        let payment_secret = PaymentSecret([42u8; 32]);

        let invoice = InvoiceBuilder::new(Currency::Bitcoin)
            .description("Trampoline this".into())
            .amount_milli_satoshis(invoice_amount())
            .payment_hash(payment_hash())
            .payment_secret(payment_secret)
            .current_timestamp()
            .min_final_cltv_expiry_delta(144)
            .build_signed(|hash| Secp256k1::new().sign_ecdsa_recoverable(hash, &private_key))
            .unwrap();

        invoice.to_string()
    }

    fn local_pubkey() -> PublicKey {
        let private_key = SecretKey::from_slice(
            &[
                0xe1, 0x26, 0xf6, 0x8f, 0x7e, 0xaf, 0xcc, 0x8b, 0x74, 0xf5, 0x4d, 0x26, 0x9f, 0xe2,
                0x06, 0xbe, 0x71, 0x50, 0x00, 0xf9, 0x4d, 0xac, 0x06, 0x7d, 0x1c, 0x04, 0xa8, 0xca,
                0x3b, 0x2d, 0xb7, 0x34,
            ][..],
        )
        .unwrap();
        PublicKey::from_secret_key(&Secp256k1::new(), &private_key)
    }

    fn mpp_timeout() -> Duration {
        Duration::from_millis(50)
    }

    fn payment_hash() -> sha256::Hash {
        sha256::Hash::hash(&preimage())
    }

    fn payment_provider() -> MockPaymentProvider {
        MockPaymentProvider::new()
    }

    fn preimage() -> [u8; 32] {
        [0u8; 32]
    }

    fn policy() -> TrampolineRoutingPolicy {
        TrampolineRoutingPolicy {
            cltv_expiry_delta: 586,
            fee_base_msat: 0,
            fee_proportional_millionths: 5000,
        }
    }

    fn request(amount_msat: u64) -> HtlcAcceptedRequest {
        HtlcAcceptedRequest {
            htlc: Htlc {
                amount_msat,
                id: 0,
                cltv_expiry: 586,
                cltv_expiry_relative: 586,
                payment_hash: (0..32).collect(),
                short_channel_id: "0x0x0".parse().unwrap(),
            },
            onion: Onion {
                forward_msat: amount_msat,
                outgoing_cltv_value: 0,
                payload: vec![TlvEntry {
                    typ: 33001,
                    value: invoice_bytes(),
                }]
                .into(),
                short_channel_id: None,
                total_msat: Some(sender_amount()),
            },
        }
    }

    fn sender_amount() -> u64 {
        1_005_000
    }

    fn temporary_trampoline_failure() -> Vec<u8> {
        HtlcFailReason::TemporaryTrampolineFailure(policy()).encode()
    }

    #[tokio::test]
    async fn test_regular_forward() {
        let payment_provider = payment_provider();
        let manager = htlc_manager(payment_provider);
        let mut request = request(sender_amount());
        request.onion.short_channel_id = Some("0x0x0".parse().unwrap());

        let result = manager.handle_htlc(&request).await;

        assert!(matches!(result, HtlcAcceptedResponse::Continue { payload } if payload.eq(&None)))
    }

    #[tokio::test]
    async fn test_receive_without_invoice() {
        let payment_provider = payment_provider();
        let manager = htlc_manager(payment_provider);
        let mut request = request(sender_amount());
        request.onion.payload = SerializedTlvStream::from(vec![]);

        let result = manager.handle_htlc(&request).await;

        assert!(matches!(result, HtlcAcceptedResponse::Continue { payload } if payload.eq(&None)))
    }

    #[tokio::test]
    async fn test_single_htlc_success() {
        let mut payment_provider = payment_provider();
        let pay_result = Ok(preimage().to_vec());
        payment_provider.expect_pay().return_once(|_| pay_result);
        let manager = htlc_manager(payment_provider);

        let result = manager.handle_htlc(&request(sender_amount())).await;

        let preimage = preimage().to_vec();
        assert!(
            matches!(result, HtlcAcceptedResponse::Resolve { payment_key } if payment_key.eq(&preimage))
        );
        let payments = manager.payments.lock().await;
        assert_eq!(0, payments.len())
    }

    #[tokio::test]
    async fn test_single_htlc_overpay_success() {
        let mut payment_provider = payment_provider();
        let pay_result = Ok(preimage().to_vec());
        let sender_amount = sender_amount() + 1;

        // Note the fee scales with the sender's overpayment
        let max_fee_msat = sender_amount - invoice_amount();
        payment_provider
            .expect_pay()
            .with(eq(PaymentRequest {
                bolt11: invoice_string(),
                amount_msat: None,
                max_fee_msat,
            }))
            .return_once(|_| pay_result);
        let manager = htlc_manager(payment_provider);

        let result = manager.handle_htlc(&request(sender_amount)).await;

        let preimage = preimage().to_vec();
        assert!(
            matches!(result, HtlcAcceptedResponse::Resolve { payment_key } if payment_key.eq(&preimage))
        );
        let payments = manager.payments.lock().await;
        assert_eq!(0, payments.len())
    }

    #[tokio::test]
    async fn test_single_htlc_payment_failure() {
        let mut payment_provider = payment_provider();
        let pay_result = Err(anyhow!("payment failed"));
        payment_provider.expect_pay().return_once(|_| pay_result);
        let manager = htlc_manager(payment_provider);

        let result = manager.handle_htlc(&request(sender_amount())).await;

        let failure = temporary_trampoline_failure();
        assert!(
            matches!(result, HtlcAcceptedResponse::Fail { failure_message } if failure_message.eq(&failure))
        );
        let payments = manager.payments.lock().await;
        assert_eq!(0, payments.len())
    }

    #[tokio::test]
    async fn test_single_htlc_too_low_times_out() {
        let manager = htlc_manager(payment_provider());
        let amount = sender_amount() - 1;

        let start = tokio::time::Instant::now();
        let result = manager.handle_htlc(&request(amount)).await;
        let end = tokio::time::Instant::now();

        let elapsed = end - start;
        assert!(elapsed.ge(&mpp_timeout()));

        let failure = temporary_trampoline_failure();
        assert!(
            matches!(result, HtlcAcceptedResponse::Fail { failure_message } if failure_message.eq(&failure))
        );
    }

    #[tokio::test]
    async fn test_two_htlc_success() {
        let mut payment_provider = payment_provider();
        let pay_result = Ok(preimage().to_vec());
        payment_provider.expect_pay().return_once(|_| pay_result);
        let manager = htlc_manager(payment_provider);
        let amount1 = 1;
        let amount2 = sender_amount() - amount1;

        let result1 = manager.handle_htlc(&request(amount1)).await;
        let result2 = manager.handle_htlc(&request(amount2)).await;

        let preimage = preimage().to_vec();
        assert!(
            matches!(result1, HtlcAcceptedResponse::Resolve { payment_key } if payment_key.eq(&preimage))
        );
        assert!(
            matches!(result2, HtlcAcceptedResponse::Resolve { payment_key } if payment_key.eq(&preimage))
        );
        let payments = manager.payments.lock().await;
        assert_eq!(0, payments.len())
    }

    #[tokio::test]
    async fn test_two_htlc_failure() {
        let mut payment_provider = payment_provider();
        let pay_result = Err(anyhow!("Payment failed"));
        payment_provider.expect_pay().return_once(|_| pay_result);
        let manager = htlc_manager(payment_provider);
        let amount1 = 1;
        let amount2 = sender_amount() - amount1;

        let result1 = manager.handle_htlc(&request(amount1)).await;
        let result2 = manager.handle_htlc(&request(amount2)).await;

        let failure = temporary_trampoline_failure();
        assert!(
            matches!(result1, HtlcAcceptedResponse::Fail { failure_message } if failure_message.eq(&failure))
        );
        assert!(
            matches!(result2, HtlcAcceptedResponse::Fail { failure_message } if failure_message.eq(&failure))
        );
        let payments = manager.payments.lock().await;
        assert_eq!(0, payments.len())
    }
}
