use std::{collections::HashMap, str::from_utf8, sync::Arc, time::Duration};

use lightning_invoice::Bolt11Invoice;
use secp256k1::PublicKey;
use tokio::sync::{mpsc, oneshot, Mutex};

use anyhow::{anyhow, Result};
use secp256k1::hashes::sha256::Hash;
use tracing::{debug, error, field, instrument, trace};

use crate::{
    block_watcher::BlockProvider,
    messages::{
        HtlcAcceptedRequest, HtlcAcceptedResponse, TrampolineInfo, TrampolineRoutingPolicy,
    },
    payment_provider::{PaymentProvider, PaymentRequest},
    store::Datastore,
};

/// HtlcManager is the main handler for htlcs. It aggregates htlcs into payments
/// based on the payment hash.
pub struct HtlcManager<B, P, S>
where
    B: BlockProvider,
    P: PaymentProvider,
    S: Datastore,
{
    params: Arc<HtlcManagerParams<B, P, S>>,
    payments: Arc<Mutex<HashMap<Hash, PaymentState>>>,
}

/// Constructor parameters for `HtlcManager`.
pub struct HtlcManagerParams<B, P, S>
where
    B: BlockProvider,
    P: PaymentProvider,
    S: Datastore,
{
    /// Value indicating whether to allow trampoline payments for invoices where
    /// the current local node is in the route hint.
    pub allow_self_route_hints: bool,

    /// Provides the current chain tip.
    pub block_provider: Arc<B>,

    /// The cltv delta to use between incoming and outgoing payments.
    pub cltv_delta: u16,

    /// The public key of the local node identity.
    pub local_pubkey: PublicKey,

    /// Timeout before multipart payments that don't add up to the right amount
    /// are failed back.
    pub mpp_timeout: Duration,

    /// Provider for payment sending.
    pub payment_provider: Arc<P>,

    /// The trampoline routing policy to enforce.
    pub routing_policy: TrampolineRoutingPolicy,

    /// Store for persisting trampoline payment states.
    pub store: Arc<S>,
}

/// Main implementation of HtlcManager.
impl<B, P, S> HtlcManager<B, P, S>
where
    B: BlockProvider + Send + Sync + 'static,
    P: PaymentProvider + Send + Sync + 'static,
    S: Datastore + Send + Sync + 'static,
{
    /// Initializes a new HtlcManager.
    pub fn new(params: HtlcManagerParams<B, P, S>) -> Self {
        Self {
            params: Arc::new(params),
            payments: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Main htlc handler. Whenever there's a new htlc added by cln, this
    /// function will do everything necessary to return the correct result. When
    /// a payment consists of multiple htlcs, this function will aggregate the
    /// htlcs into a payment and return a result for every associated htlc
    /// simultaneously.
    #[instrument(
        level = "debug",
        skip_all,
        fields(
            payment_hash = %hex::encode(&req.htlc.payment_hash),
            short_channel_id = %req.htlc.short_channel_id,
            htlc_id = req.htlc.id))]
    pub async fn handle_htlc(&self, req: &HtlcAcceptedRequest) -> HtlcAcceptedResponse {
        let (sender, receiver) = oneshot::channel();
        let trampoline = match self.check_htlc(req) {
            HtlcCheckResult::Response(resp) => return resp,
            HtlcCheckResult::Trampoline(trampoline) => *trampoline,
        };

        {
            let mut payments = self.payments.lock().await;
            let payment_state = payments
                .entry(*trampoline.invoice.payment_hash())
                .or_insert_with(|| {
                    // If the payment did not yet exist, spawn the payment lifecycle.
                    let (s1, r1) = mpsc::channel(1);
                    let (s2, r2) = mpsc::channel(1);
                    tokio::spawn(payment_lifecycle(
                        Arc::clone(&self.params),
                        Arc::clone(&self.payments),
                        trampoline.clone(),
                        r1,
                        r2,
                    ));

                    // And insert the payment into the hashmap.
                    PaymentState::new(trampoline.clone(), s1, s2)
                });

            // If the trampoline info doesn't match previous trampoline infos,
            // fail the payment asap.
            if trampoline != payment_state.trampoline {
                trace!("Trampoline info doesn't match existing trampoline info for payment.");
                payment_state
                    .fail(self.temporary_trampoline_failure())
                    .await;
            }

            // Ensure there's enough relative time to claim htlcs.
            if req.htlc.cltv_expiry_relative < self.params.routing_policy.cltv_expiry_delta as i64 {
                trace!(
                    cltv_expiry_relative = req.htlc.cltv_expiry_relative,
                    policy_cltv_expiry_delta = self.params.routing_policy.cltv_expiry_delta,
                    "Relative cltv expiry too low."
                );
                payment_state
                    .fail(self.temporary_trampoline_failure())
                    .await;
            }

            // The total is either set in the onion, or it's the forward amount of
            // the current htlc.
            let total_msat = match req.onion.total_msat {
                Some(total_msat) => total_msat,
                None => req.onion.forward_msat,
            };

            // Ensure enough fees are paid according to the routing policy.
            if !self
                .params
                .routing_policy
                .fee_sufficient(total_msat, trampoline.amount_msat)
            {
                trace!(
                    total_msat = total_msat,
                    trampoline.amount_msat = trampoline.amount_msat,
                    policy = field::debug(&self.params.routing_policy),
                    "Payment offers too low fee for trampoline."
                );
                payment_state
                    .fail(self.temporary_trampoline_failure())
                    .await;
            }

            // Do add the htlc to the payment state always, also if it has
            // failed. It could be a payment was already in-flight, so
            // eventually this htlc settles.
            payment_state.add_htlc(req, sender).await;
        }

        let resp = receiver.await.unwrap();
        debug!("returning {:?}", resp);
        resp
    }

    /// Checks whether this htlc belongs to a trampoline payment and returns the
    /// relevant info if so, otherwise returns the appropriate result for the
    /// htlc.
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
        if trampoline.payee.eq(&self.params.local_pubkey) {
            trace!("We are the payee, invoice subsystem handles this htlc.");
            return HtlcCheckResult::Response(default_response());
        }

        // Check whether we are the last hop in the route hint. We can't rewrite
        // this as a forward, so we'll error if that's the case.
        let route_hints = trampoline.invoice.route_hints();
        if let Some(our_hint) = route_hints.iter().find(|hint| {
            hint.0
                .last()
                .map(|hop| hop.src_node_id.eq(&self.params.local_pubkey))
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

            if !self.params.allow_self_route_hints {
                debug!(
                    "Got invoice with ourselves in the hint. Erroring because this is not supported."
                );
                return HtlcCheckResult::Response(HtlcAcceptedResponse::temporary_node_failure());
            }
        }

        debug!("This is a trampoline payment.");
        HtlcCheckResult::Trampoline(Box::new(trampoline))
    }

    /// Extracts the trampoline information from the request onion payload.
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
            routing_policy: self.params.routing_policy.clone(),
            amount_msat,
            bolt11: String::from(invoice_str),
            invoice,
            payee,
        }))
    }

    /// Convenience function to return temporary trampoline failure with the
    /// current routing policy.
    fn temporary_trampoline_failure(&self) -> HtlcAcceptedResponse {
        HtlcAcceptedResponse::temporary_trampoline_failure(self.params.routing_policy.clone())
    }
}

/// The default response is continue, meaning the htlc is not modified by this
/// plugin.
fn default_response() -> HtlcAcceptedResponse {
    HtlcAcceptedResponse::Continue { payload: None }
}

/// The main lifecycle of a payment. Should be started in the background when
/// the first htlc for the given payment hash comes in.
#[instrument(
    level = "debug",
    skip_all,
    fields(payment_hash = %trampoline.invoice.payment_hash()))]
async fn payment_lifecycle<B, P, S>(
    params: Arc<HtlcManagerParams<B, P, S>>,
    payments: Arc<Mutex<HashMap<Hash, PaymentState>>>,
    trampoline: TrampolineInfo,
    mut payment_ready: mpsc::Receiver<()>,
    mut fail_requested: mpsc::Receiver<HtlcAcceptedResponse>,
) where
    B: BlockProvider,
    P: PaymentProvider,
    S: Datastore,
{
    let state = match params.store.fetch_payment_info(&trampoline).await {
        Ok(state) => state,
        Err(e) => {
            error!("Failed to fetch payment info: {:?}", e);
            resolve(
                &payments,
                &trampoline,
                HtlcAcceptedResponse::temporary_node_failure(),
            )
            .await;
            return;
        }
    };

    let time_left = match state {
        crate::store::PaymentState::Free => params.mpp_timeout,
        crate::store::PaymentState::Pending { attempt_id, attempt_time_seconds } => {
            match params
                .payment_provider
                .wait_payment(*trampoline.invoice.payment_hash())
                .await
            {
                Ok(maybe_preimage) => {
                    if let Some(preimage) = maybe_preimage {
                        resolve(
                            &payments,
                            &trampoline,
                            HtlcAcceptedResponse::resolve(preimage.clone()),
                        )
                        .await;
                        match params
                            .store
                            .mark_succeeded(&trampoline, attempt_id, preimage)
                            .await
                        {
                            Ok(_) => {}
                            Err(e) => {
                                error!("Failed to mark payment as succeeded: {:?}", e);
                            }
                        }
                        return;
                    }

                    // Get the time left since this attempt was started.
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .saturating_sub(Duration::from_secs(attempt_time_seconds))
                }
                Err(e) => {
                    error!("Failed to await pending payment: {:?}", e);

                    // TODO: Now what? Apparently we have a pending payment, so
                    // we shouldn't return here, but there is also nothing else
                    // possible to do! Should we panic?
                    todo!("Failed to await pending payment, but cannot resolve yet, because it's pending.");
                }
            }
        }
        crate::store::PaymentState::Succeeded { preimage } => {
            resolve(
                &payments,
                &trampoline,
                HtlcAcceptedResponse::resolve(preimage),
            )
            .await;
            return;
        }
    };

    tokio::select! {
        _ = tokio::time::sleep(time_left) => {
            debug!("Payment timed out waiting for htlcs.");
            // TODO: Double-check no payment is in-flight.
            resolve(&payments, &trampoline, HtlcAcceptedResponse::temporary_trampoline_failure(
                trampoline.routing_policy.clone())).await;
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
            resolve(&payments, &trampoline, failure).await;
            return;
        }
        _ = payment_ready.recv() => {
            debug!("Received payment ready.");
        }
    };

    // Amount is only set if not set in the invoice.
    let amount_msat = match trampoline.invoice.amount_milli_satoshis() {
        Some(_) => None,
        None => Some(trampoline.amount_msat),
    };

    // Get the payment parameters.
    let (max_fee_msat, cltv_expiry) = {
        let payments = payments.lock().await;
        let payment = payments
            .get(trampoline.invoice.payment_hash())
            .expect("Payment is ready for paying, but payment was already gone.");
        let max_fee_msat = payment
            .amount_received_msat
            .saturating_sub(trampoline.amount_msat);
        (max_fee_msat, payment.cltv_expiry)
    };

    // Compute the maximum cltv delta to be safe for this payment. We always
    // want to keep a minimum gap of size `cltv_delta` between the incoming and
    // outgoing expiry in order to handle force closures gracefully. The cltv
    // delta is always capped by the one defined in our trampoline policy.
    let current_height = params.block_provider.current_height().await;
    let max_cltv_delta = std::cmp::min(
        cltv_expiry
            .saturating_sub(current_height)
            .saturating_sub(params.cltv_delta as u32)
            .try_into()
            .unwrap_or(u16::MAX),
        trampoline.routing_policy.cltv_expiry_delta,
    );

    // Add the payment attempt to the data store. This allows us to fetch
    // payment information in case of a restart when a payment is in-flight.
    let attempt_id = match params.store.add_payment_attempt(&trampoline).await {
        Ok(attempt_id) => attempt_id,
        Err(e) => {
            // If we cannot persist the payment, error and return. Too risky.
            error!("Failed to insert payment attempt in data store: {:?}", e);
            resolve(
                &payments,
                &trampoline,
                HtlcAcceptedResponse::temporary_node_failure(),
            )
            .await;
            return;
        }
    };

    trace!("about to pay.");
    // Send the payment. This method will return once the payment has fully
    // resolved.
    let pay_result = params
        .payment_provider
        .pay(PaymentRequest {
            bolt11: trampoline.bolt11.clone(),
            payment_hash: *trampoline.invoice.payment_hash(),
            amount_msat,
            max_fee_msat,
            max_cltv_delta,
        })
        .await;
    trace!("pay returned.");

    match pay_result {
        Ok(preimage) => {
            debug!("Payment succeeded, resolving payment with preimage.");
            resolve(
                &payments,
                &trampoline,
                HtlcAcceptedResponse::Resolve {
                    payment_key: preimage.clone(),
                },
            )
            .await;
            if let Err(e) = params
                .store
                .mark_succeeded(&trampoline, attempt_id.attempt_id, preimage)
                .await
            {
                error!("Failed to mark payment as succeeded: {:?}", e);
            }
        }
        Err(e) => {
            debug!("Payment failed: {:?}", e);
            // TODO: Extract relevant info from the error?
            resolve(
                &payments,
                &trampoline,
                HtlcAcceptedResponse::temporary_trampoline_failure(
                    trampoline.routing_policy.clone(),
                ),
            )
            .await;
            if let Err(e) = params.store.mark_failed(&trampoline, &attempt_id).await {
                error!("Failed to mark payment as failed: {:?}", e);
            }
        }
    }
}

/// Resolves the payment with the given response. Removes the payment from the
/// hashmap and returns a response for every associated htlc.
async fn resolve(
    payments: &Arc<Mutex<HashMap<Hash, PaymentState>>>,
    trampoline: &TrampolineInfo,
    resp: HtlcAcceptedResponse,
) {
    let mut payments = payments.lock().await;
    let mut payment = payments
        .remove(trampoline.invoice.payment_hash())
        .expect("Expected to resolve payment, but payment was already gone.");
    payment.resolve(resp)
}

/// The current state of a given payment.
struct PaymentState {
    /// Htlc listeners waiting for a response for the current payment.
    htlcs: Vec<oneshot::Sender<HtlcAcceptedResponse>>,

    /// The trampoline information extracted from the htlcs.
    trampoline: TrampolineInfo,

    /// Value indicating whether the payment is ready for paying.
    is_ready: bool,

    /// Signal invoked when the payment is ready for paying.
    payment_ready: mpsc::Sender<()>,

    /// When the payment should be failed back, this signals the payment
    /// lifecycle the payment should be failed. If the payment is currently in
    /// the process of being paid, this might be ignored.
    fail_requested: mpsc::Sender<HtlcAcceptedResponse>,

    /// The total amount currently received with htlcs with the same payment
    /// hash.
    amount_received_msat: u64,

    /// The minimum cltv expiry on htlcs associated to this payment.
    cltv_expiry: u32,

    /// If the payment was already resolved, this contains the resolution.
    resolution: Option<HtlcAcceptedResponse>,
}

impl PaymentState {
    /// Initializes a new blank payment state.
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

    /// Adds a htlc to the payment, incrementing the amount received. If the
    /// added htlc makes the payment add up to the expected amount, signals the
    /// payment is ready.
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

    /// Requests failure for this payment. Does not fail back htlcs immediately,
    /// because an outgoing payment may already be in-flight.
    async fn fail(&self, resp: HtlcAcceptedResponse) {
        let _ = self.fail_requested.send(resp).await;
    }

    /// Resolves all htlcs associated to this payment with the given resolution
    /// immediately.
    fn resolve(&mut self, resp: HtlcAcceptedResponse) {
        trace!(resolution = field::debug(&resp), "resolving payment");
        self.resolution = Some(resp.clone());

        while let Some(listener) = self.htlcs.pop() {
            match listener.send(resp.clone()) {
                Ok(_) => {}
                Err(e) => error!("htlc listener hung up, could not send response {:?}", e),
            };
        }
    }
}

enum HtlcCheckResult {
    Response(HtlcAcceptedResponse),
    Trampoline(Box<TrampolineInfo>),
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
    use tokio::join;
    use tracing_test::traced_test;

    use crate::{
        block_watcher::MockBlockProvider,
        htlc_manager::HtlcManager,
        messages::{
            Htlc, HtlcAcceptedRequest, HtlcAcceptedResponse, HtlcFailReason, Onion,
            TrampolineRoutingPolicy,
        },
        payment_provider::{MockPaymentProvider, PaymentRequest},
        store::{AttemptId, MockDatastore},
        tlv::{SerializedTlvStream, TlvEntry},
    };

    use super::HtlcManagerParams;

    fn cltv_delta() -> u16 {
        34
    }

    fn htlc_manager(
        payment_provider: MockPaymentProvider,
    ) -> HtlcManager<MockBlockProvider, MockPaymentProvider, MockDatastore> {
        let mut block_provider = MockBlockProvider::new();
        block_provider.expect_current_height().returning(|| 0);
        let mut store = MockDatastore::new();
        store
            .expect_fetch_payment_info()
            .return_once(|_| Ok(crate::store::PaymentState::Free));
        store.expect_add_payment_attempt().returning(|_| {
            Ok(AttemptId {
                attempt_id: String::from("0"),
                state_generation: 0,
            })
        });
        store.expect_mark_failed().returning(|_, _| Ok(()));
        store.expect_mark_succeeded().returning(|_, _, _| Ok(()));
        HtlcManager::new(HtlcManagerParams {
            allow_self_route_hints: true,
            block_provider: Arc::new(block_provider),
            cltv_delta: cltv_delta(),
            local_pubkey: local_pubkey(),
            mpp_timeout: mpp_timeout(),
            payment_provider: Arc::new(payment_provider),
            routing_policy: policy(),
            store: Arc::new(store),
        })
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
    #[traced_test]
    async fn test_regular_forward() {
        let payment_provider = payment_provider();
        let manager = htlc_manager(payment_provider);
        let mut request = request(sender_amount());
        request.onion.short_channel_id = Some("0x0x0".parse().unwrap());

        let result = manager.handle_htlc(&request).await;

        assert!(matches!(result, HtlcAcceptedResponse::Continue { payload } if payload.eq(&None)))
    }

    #[tokio::test]
    #[traced_test]
    async fn test_receive_without_invoice() {
        let payment_provider = payment_provider();
        let manager = htlc_manager(payment_provider);
        let mut request = request(sender_amount());
        request.onion.payload = SerializedTlvStream::from(vec![]);

        let result = manager.handle_htlc(&request).await;

        assert!(matches!(result, HtlcAcceptedResponse::Continue { payload } if payload.eq(&None)))
    }

    #[tokio::test]
    #[traced_test]
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
    #[traced_test]
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
                payment_hash: payment_hash(),
                amount_msat: None,
                max_fee_msat,
                max_cltv_delta: policy().cltv_expiry_delta - cltv_delta(),
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
    #[traced_test]
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
    #[traced_test]
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
    #[traced_test]
    async fn test_two_htlc_success() {
        let mut payment_provider = payment_provider();
        let pay_result = Ok(preimage().to_vec());
        payment_provider.expect_pay().return_once(|_| pay_result);
        let manager = htlc_manager(payment_provider);
        let amount1 = 1;
        let amount2 = sender_amount() - amount1;
        let request1 = request(amount1);
        let request2 = request(amount2);

        let (result1, result2) = join!(
            manager.handle_htlc(&request1),
            manager.handle_htlc(&request2)
        );

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
    #[traced_test]
    async fn test_two_htlc_failure() {
        let mut payment_provider = payment_provider();
        let pay_result = Err(anyhow!("Payment failed"));
        payment_provider.expect_pay().return_once(|_| pay_result);
        let manager = htlc_manager(payment_provider);
        let amount1 = 1;
        let amount2 = sender_amount() - amount1;
        let request1 = request(amount1);
        let request2 = request(amount2);

        let (result1, result2) = join!(
            manager.handle_htlc(&request1),
            manager.handle_htlc(&request2)
        );

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
