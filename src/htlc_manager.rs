use std::{collections::HashMap, str::from_utf8, sync::Arc, time::Duration};

use lightning_invoice::Bolt11Invoice;
use secp256k1::PublicKey;
use tokio::sync::{mpsc, oneshot, Mutex};

use anyhow::{anyhow, Context, Result};
use secp256k1::hashes::sha256::Hash;
use tracing::{debug, error, field, instrument, trace, warn};

use crate::{
    block_watcher::BlockProvider,
    email::{NotificationService, NotifyPaymentFailedRequest},
    messages::{
        HtlcAcceptedRequest, HtlcAcceptedResponse, TrampolineInfo, TrampolineRoutingPolicy,
    },
    payment_provider::{PaymentProvider, PaymentRequest},
    store::Datastore,
    tlv::{FromBytes, ProtoBuf, SerializedTlvStream, ToBytes},
};

const TLV_PAYMENT_METADATA: u64 = 16;
const TLV_TRAMPOLINE_INVOICE: u64 = 33001;
const TLV_TRAMPOLINE_AMOUNT: u64 = 33003;

/// HtlcManager is the main handler for htlcs. It aggregates htlcs into payments
/// based on the payment hash.
pub struct HtlcManager<B, N, P, S>
where
    B: BlockProvider,
    N: NotificationService,
    P: PaymentProvider,
    S: Datastore,
{
    params: Arc<HtlcManagerParams<B, N, P, S>>,
    payments: Arc<Mutex<HashMap<Hash, PaymentState>>>,
}

/// Constructor parameters for `HtlcManager`.
pub struct HtlcManagerParams<B, N, P, S>
where
    B: BlockProvider,
    N: NotificationService,
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

    /// Service for notifying failed payments.
    pub notification_service: Arc<N>,

    /// Provider for payment sending.
    pub payment_provider: Arc<P>,

    /// The trampoline routing policy to enforce.
    pub routing_policy: TrampolineRoutingPolicy,

    /// Store for persisting trampoline payment states.
    pub store: Arc<S>,
}

/// Main implementation of HtlcManager.
impl<B, N, P, S> HtlcManager<B, N, P, S>
where
    B: BlockProvider + Send + Sync + 'static,
    N: NotificationService + Send + Sync + 'static,
    P: PaymentProvider + Send + Sync + 'static,
    S: Datastore + Send + Sync + 'static,
{
    /// Initializes a new HtlcManager.
    pub fn new(params: HtlcManagerParams<B, N, P, S>) -> Self {
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
        trace!("got htlc");
        let (sender, receiver) = oneshot::channel();
        let trampoline = match self.check_htlc(req) {
            HtlcCheckResult::Response(resp) => return resp,
            HtlcCheckResult::Trampoline(trampoline) => *trampoline,
        };
        let forward_msat = match req.onion.forward_msat {
            Some(forward_msat) => forward_msat,
            None => {
                trace!("onion missing forward_msat, skipping");
                return default_response(req);
            }
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
                    .fail(HtlcAcceptedResponse::temporary_trampoline_failure())
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
                    .fail(self.trampoline_fee_or_expiry_insufficient())
                    .await;
            }

            // The total is either set in the onion, or it's the forward amount of
            // the current htlc.
            let total_msat = match req.onion.total_msat {
                Some(total_msat) => total_msat,
                None => forward_msat,
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
                    .fail(self.trampoline_fee_or_expiry_insufficient())
                    .await;
            }

            // Do add the htlc to the payment state always, also if it has
            // failed. It could be a payment was already in-flight, so
            // eventually this htlc settles.
            payment_state.add_htlc(req, sender).await;
        }

        let resp = receiver
            .await
            .context("receiver defined in the same function is gone")
            .unwrap();
        debug!("returning {:?}", resp);
        resp
    }

    /// Checks whether this htlc belongs to a trampoline payment and returns the
    /// relevant info if so, otherwise returns the appropriate result for the
    /// htlc.
    /// Note this function only checks whether the htlc is a trampoline htlc. It
    /// does not check vltc expiry or fee settings, because they may change when
    /// the node is restarted and the htlc is replayed. A payment may already be
    /// in-flight All those checks should be done in `handle_htlc`, making the
    /// payment lifecycle resposible for failing back the htlcs.
    fn check_htlc(&self, req: &HtlcAcceptedRequest) -> HtlcCheckResult {
        // For trampoline payments we appear to be the destination. If we're not the
        // destination, continue.
        if req.onion.short_channel_id.is_some() {
            trace!("This is a forward, returning continue.");
            return HtlcCheckResult::Response(default_response(req));
        }

        // Check whether this is a trampoline payment by extracting the invoice.
        let trampoline = match self.extract_trampoline_info(req) {
            Ok(trampoline) => match trampoline {
                Some(trampoline) => trampoline,
                None => {
                    trace!("This is a not a trampoline htlc, returning continue.");
                    return HtlcCheckResult::Response(default_response(req));
                }
            },
            Err(e) => {
                debug!("Failed to extract trampoline info from htlc: {:?}", e);
                return HtlcCheckResult::Response(default_response(req));
            }
        };

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
        let payment_metadata: SerializedTlvStream =
            match req.onion.payload.get(TLV_PAYMENT_METADATA) {
                Some(payment_metadata) => {
                    match SerializedTlvStream::from_bytes(payment_metadata.value) {
                        Ok(payment_metadata) => payment_metadata,
                        Err(e) => {
                            warn!("htlc had invalid payment metadata: {:?}", e);
                            return Ok(None);
                        }
                    }
                }
                None => {
                    trace!("htlc does not have payment metadata.");
                    return Ok(None);
                }
            };

        let invoice_blob = match payment_metadata.get(TLV_TRAMPOLINE_INVOICE) {
            Some(invoice_blob) => invoice_blob.value,
            None => {
                trace!("payment metadata does not contain invoice.");
                return Ok(None);
            }
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
        let tlv_amount_msat = match payment_metadata.get(TLV_TRAMPOLINE_AMOUNT) {
            Some(amount_blob) => {
                let mut b: bytes::Bytes = amount_blob.value.into();
                match b.get_tu64() {
                    Ok(amount_msat) => Some(amount_msat),
                    Err(e) => {
                        debug!("Got invalid amount of len {} in htlc TLV: {:?}", b.len(), e);
                        None
                    }
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
                            "non-matching amounts in invoice tlv {} and amount tlv {}",
                            invoice_amount_msat,
                            tlv_amount_msat
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

    /// Convenience function to return trampoline_fee_or_expiry_insufficient
    /// with the current routing policy.
    fn trampoline_fee_or_expiry_insufficient(&self) -> HtlcAcceptedResponse {
        HtlcAcceptedResponse::trampoline_fee_or_expiry_insufficient(
            self.params.routing_policy.clone(),
        )
    }
}

/// The default response is continue, meaning the htlc is not modified by this
/// plugin.
fn default_response(req: &HtlcAcceptedRequest) -> HtlcAcceptedResponse {
    if let Some(payment_metadata) = req.onion.payload.get(TLV_PAYMENT_METADATA) {
        if let Ok(payment_metadata) =
            TryInto::<SerializedTlvStream>::try_into(payment_metadata.value)
        {
            if payment_metadata.get(TLV_TRAMPOLINE_INVOICE).is_some()
                || payment_metadata.get(TLV_TRAMPOLINE_AMOUNT).is_some()
            {
                let mut payload = req.onion.payload.clone();
                payload.remove(TLV_PAYMENT_METADATA);
                return HtlcAcceptedResponse::Continue {
                    payload: Some(SerializedTlvStream::to_bytes(payload)),
                };
            }
        }
    }
    HtlcAcceptedResponse::Continue { payload: None }
}

/// The main lifecycle of a payment. Should be started in the background when
/// the first htlc for the given payment hash comes in.
#[instrument(
    level = "debug",
    skip_all,
    fields(payment_hash = %trampoline.invoice.payment_hash()))]
async fn payment_lifecycle<B, N, P, S>(
    params: Arc<HtlcManagerParams<B, N, P, S>>,
    payments: Arc<Mutex<HashMap<Hash, PaymentState>>>,
    trampoline: TrampolineInfo,
    mut payment_ready: mpsc::Receiver<()>,
    mut fail_requested: mpsc::Receiver<HtlcAcceptedResponse>,
) where
    B: BlockProvider,
    N: NotificationService,
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
        crate::store::PaymentState::Pending {
            attempt_id,
            attempt_time_seconds,
        } => {
            debug!(
                attempt_id.attempt_id,
                attempt_time_seconds, "Payment is pending, awaiting payment."
            );
            match params
                .payment_provider
                .wait_payment(*trampoline.invoice.payment_hash())
                .await
            {
                Ok(maybe_preimage) => {
                    if let Some(preimage) = maybe_preimage {
                        trace!("pending payment resolved with preimage");
                        resolve(
                            &payments,
                            &trampoline,
                            HtlcAcceptedResponse::resolve(preimage.clone()),
                        )
                        .await;
                        match params
                            .store
                            .mark_succeeded(&trampoline, &attempt_id, preimage)
                            .await
                        {
                            Ok(_) => {}
                            Err(e) => {
                                error!("Failed to mark payment as succeeded: {:?}", e);
                            }
                        }
                        return;
                    }

                    trace!("pending payment resolved without preimage");
                    match params.store.mark_failed(&trampoline, &attempt_id).await {
                        Ok(_) => {}
                        Err(e) => {
                            error!("Failed to mark payment as failed: {:?}", e);
                            resolve(
                                &payments,
                                &trampoline,
                                HtlcAcceptedResponse::temporary_node_failure(),
                            )
                            .await;
                            return;
                        }
                    }
                    // Get the time left since this attempt was started. Note
                    // that this is not really the mpp timeout time remaining,
                    // but it's the mpp timeout minus the start of the payment
                    // attempt. This is the best we can do without storing the
                    // start time of every single htlc when it arrives. This
                    // check is mainly here to not let restarts reset the mpp
                    // timeout entirely.
                    params.mpp_timeout.saturating_sub(
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .context("duration since unix epoch should always work")
                            .unwrap()
                            .saturating_sub(Duration::from_secs(attempt_time_seconds)),
                    )
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
            debug!("existing payment already had preimage");
            resolve(
                &payments,
                &trampoline,
                HtlcAcceptedResponse::resolve(preimage),
            )
            .await;
            return;
        }
    };

    if time_left.is_zero() {
        debug!("MPP timeout has expired.");
        resolve(
            &payments,
            &trampoline,
            HtlcAcceptedResponse::temporary_trampoline_failure(),
        )
        .await;
        return;
    }

    tokio::select! {
        _ = tokio::time::sleep(time_left) => {
            debug!("Payment timed out waiting for htlcs.");
            resolve(&payments, &trampoline, HtlcAcceptedResponse::temporary_trampoline_failure()).await;
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
                .mark_succeeded(&trampoline, &attempt_id, preimage)
                .await
            {
                error!("Failed to mark payment as succeeded: {:?}", e);
            }
        }
        Err(e) => {
            debug!("Payment failed: {:?}", e);
            // TODO: Extract new trampoline policy if expiry or fee was
            // insufficient.
            resolve(
                &payments,
                &trampoline,
                HtlcAcceptedResponse::temporary_trampoline_failure(),
            )
            .await;
            if let Err(e) = params.store.mark_failed(&trampoline, &attempt_id).await {
                error!("Failed to mark payment as failed: {:?}", e);
            }

            params
                .notification_service
                .notify_payment_failed(NotifyPaymentFailedRequest {
                    destination: trampoline.payee,
                    error: e,
                    payment_hash: *trampoline.invoice.payment_hash(),
                    invoice: trampoline.bolt11,
                })
                .await;
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

    /// Value indicating whether failure is requested.
    is_fail_requested: bool,

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
            is_fail_requested: false,
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
            && !self.is_fail_requested
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
    async fn fail(&mut self, resp: HtlcAcceptedResponse) {
        if !self.is_fail_requested {
            self.is_ready = false;
            self.is_fail_requested = true;

            let _ = self.fail_requested.send(resp).await;
        }
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
    use std::{
        sync::Arc,
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    use anyhow::anyhow;
    use lightning_invoice::{
        Currency, InvoiceBuilder, PaymentSecret, RouteHint, RouteHintHop, RoutingFees,
    };
    use mockall::predicate::eq;
    use secp256k1::{
        hashes::{sha256, Hash},
        PublicKey, Secp256k1, SecretKey,
    };
    use tokio::join;
    use tracing_test::traced_test;

    use crate::{
        block_watcher::MockBlockProvider,
        email::MockNotificationService,
        htlc_manager::HtlcManager,
        messages::{
            Htlc, HtlcAcceptedRequest, HtlcAcceptedResponse, HtlcFailReason, Onion,
            TrampolineRoutingPolicy,
        },
        payment_provider::{MockPaymentProvider, PaymentRequest},
        store::{self, AttemptId, MockDatastore},
        tlv::{SerializedTlvStream, TlvEntry, ToBytes},
    };

    use super::HtlcManagerParams;

    struct TestData {
        allow_self_route_hints: bool,
        block_provider: MockBlockProvider,
        cltv_delta: u16,
        destination_privkey: SecretKey,
        invoice_amount: Option<u64>,
        invoice_route_hint: Option<RouteHint>,
        local_pubkey: PublicKey,
        mpp_timeout: Duration,
        notification_service: MockNotificationService,
        payment_provider: MockPaymentProvider,
        preimage: [u8; 32],
        policy: TrampolineRoutingPolicy,
        sender_amount: u64,
        store: MockDatastore,
        total_amount: u64,
    }

    impl TestData {
        fn default() -> Self {
            let mut block_provider = MockBlockProvider::new();
            block_provider.expect_current_height().returning(|| 0);
            let mut store = MockDatastore::new();
            store
                .expect_fetch_payment_info()
                .returning(|_| Ok(crate::store::PaymentState::Free));
            store.expect_add_payment_attempt().returning(|_| {
                Ok(AttemptId {
                    attempt_id: String::from("0"),
                    state_generation: 0,
                })
            });

            let private_key = SecretKey::from_slice(
                &[
                    0xe1, 0x26, 0xf6, 0x8f, 0x7e, 0xaf, 0xcc, 0x8b, 0x74, 0xf5, 0x4d, 0x26, 0x9f,
                    0xe2, 0x06, 0xbe, 0x71, 0x50, 0x00, 0xf9, 0x4d, 0xac, 0x06, 0x7d, 0x1c, 0x04,
                    0xa8, 0xca, 0x3b, 0x2d, 0xb7, 0x34,
                ][..],
            )
            .unwrap();
            let local_pubkey = PublicKey::from_secret_key(&Secp256k1::new(), &private_key);

            let destination_privkey = SecretKey::from_slice(
                &[
                    0xe1, 0x26, 0xf6, 0x8f, 0x7e, 0xaf, 0xcc, 0x8b, 0x74, 0xf5, 0x4d, 0x26, 0x9f,
                    0xe2, 0x06, 0xbe, 0x71, 0x50, 0x00, 0xf9, 0x4d, 0xac, 0x06, 0x7d, 0x1c, 0x04,
                    0xa8, 0xca, 0x3b, 0x2d, 0xb7, 0x35,
                ][..],
            )
            .unwrap();
            Self {
                allow_self_route_hints: true,
                block_provider,
                cltv_delta: 34,
                destination_privkey,
                invoice_amount: Some(1_000_000),
                invoice_route_hint: None,
                local_pubkey,
                mpp_timeout: Duration::from_millis(50),
                notification_service: MockNotificationService::new(),
                payment_provider: MockPaymentProvider::new(),
                preimage: preimage1(),
                policy: TrampolineRoutingPolicy {
                    cltv_expiry_delta: 1008,
                    fee_base_msat: 0,
                    fee_proportional_millionths: 5000,
                },
                sender_amount: 1_005_000,
                store,
                total_amount: 1_005_000,
            }
        }

        fn htlc_manager(
            self,
        ) -> HtlcManager<
            MockBlockProvider,
            MockNotificationService,
            MockPaymentProvider,
            MockDatastore,
        > {
            HtlcManager::new(HtlcManagerParams {
                allow_self_route_hints: self.allow_self_route_hints,
                block_provider: Arc::new(self.block_provider),
                cltv_delta: self.cltv_delta,
                local_pubkey: self.local_pubkey,
                mpp_timeout: self.mpp_timeout,
                notification_service: Arc::new(self.notification_service),
                payment_provider: Arc::new(self.payment_provider),
                routing_policy: self.policy.clone(),
                store: Arc::new(self.store),
            })
        }

        fn invoice_bytes(&self) -> Vec<u8> {
            self.invoice_string().as_bytes().to_vec()
        }

        fn invoice_string(&self) -> String {
            let payment_secret = PaymentSecret([42u8; 32]);

            let mut invoice = InvoiceBuilder::new(Currency::Bitcoin)
                .description("Trampoline this".into())
                .payment_hash(self.payment_hash())
                .payment_secret(payment_secret)
                .timestamp(SystemTime::UNIX_EPOCH)
                .min_final_cltv_expiry_delta(144);

            if let Some(amount) = self.invoice_amount {
                invoice = invoice.amount_milli_satoshis(amount)
            }

            if let Some(hint) = &self.invoice_route_hint {
                invoice = invoice.private_route(hint.clone());
            }

            let invoice = invoice
                .build_signed(|hash| {
                    Secp256k1::new().sign_ecdsa_recoverable(hash, &self.destination_privkey)
                })
                .unwrap();

            invoice.to_string()
        }

        fn payment_hash(&self) -> sha256::Hash {
            sha256::Hash::hash(&self.preimage)
        }

        fn payment_hash_vec(&self) -> Vec<u8> {
            self.payment_hash().to_byte_array().to_vec()
        }

        fn request(&self) -> HtlcAcceptedRequest {
            HtlcAcceptedRequest {
                htlc: Htlc {
                    amount_msat: self.sender_amount,
                    id: 0,
                    cltv_expiry: self.policy.cltv_expiry_delta as u32,
                    cltv_expiry_relative: self.policy.cltv_expiry_delta as i64,
                    payment_hash: self.payment_hash_vec(),
                    short_channel_id: "0x0x0".parse().unwrap(),
                },
                onion: Onion {
                    forward_msat: Some(self.sender_amount),
                    payload: construct_payload(vec![TlvEntry {
                        typ: 33001,
                        value: self.invoice_bytes(),
                    }]),
                    short_channel_id: None,
                    total_msat: Some(self.total_amount),
                },
            }
        }

        fn set_self_route_hint(&mut self) {
            self.invoice_route_hint = Some(RouteHint(vec![RouteHintHop {
                cltv_expiry_delta: 80,
                fees: RoutingFees {
                    base_msat: 1000,
                    proportional_millionths: 10,
                },
                htlc_maximum_msat: Some(1_000_000),
                htlc_minimum_msat: Some(1_000),
                short_channel_id: 0,
                src_node_id: self.local_pubkey,
            }]));
        }

        fn temporary_trampoline_failure(&self) -> Vec<u8> {
            HtlcFailReason::TemporaryTrampolineFailure.encode()
        }

        fn trampoline_fee_or_expiry_insufficient(&self) -> Vec<u8> {
            HtlcFailReason::TrampolineFeeOrExpiryInsufficient(self.policy.clone()).encode()
        }
    }

    fn preimage1() -> [u8; 32] {
        [0; 32]
    }

    fn preimage2() -> [u8; 32] {
        [1; 32]
    }

    fn construct_payload(metadata_tlvs: Vec<TlvEntry>) -> SerializedTlvStream {
        vec![TlvEntry {
            typ: 16,
            value: SerializedTlvStream::to_bytes(SerializedTlvStream::from(metadata_tlvs)),
        }]
        .into()
    }

    #[tokio::test]
    #[traced_test]
    async fn test_regular_forward() {
        let test = TestData::default();
        let mut request = test.request();
        request.onion.short_channel_id = Some("0x0x0".parse().unwrap());
        let manager = test.htlc_manager();

        let result = manager.handle_htlc(&request).await;

        assert!(matches!(result, HtlcAcceptedResponse::Continue { payload } if payload.eq(&None)));
        let payments = manager.payments.lock().await;
        assert_eq!(0, payments.len());
    }

    #[tokio::test]
    #[traced_test]
    async fn test_receive_without_invoice() {
        let test = TestData::default();
        let mut request = test.request();
        request.onion.payload = SerializedTlvStream::from(vec![]);
        let manager = test.htlc_manager();

        let result = manager.handle_htlc(&request).await;

        assert!(matches!(result, HtlcAcceptedResponse::Continue { payload } if payload.eq(&None)));
        let payments = manager.payments.lock().await;
        assert_eq!(0, payments.len());
    }

    #[tokio::test]
    #[traced_test]
    async fn test_single_htlc_success() {
        let mut test = TestData::default();
        let pay_result = Ok(test.preimage.to_vec());
        test.payment_provider
            .expect_pay()
            .return_once(|_| pay_result)
            .once();
        let preimage = test.preimage.to_vec();
        let request = test.request();
        let manager = test.htlc_manager();

        let result = manager.handle_htlc(&request).await;

        assert!(
            matches!(result, HtlcAcceptedResponse::Resolve { payment_key } if payment_key.eq(&preimage))
        );
        let payments = manager.payments.lock().await;
        assert_eq!(0, payments.len());
    }

    #[tokio::test]
    #[traced_test]
    async fn test_single_htlc_overpay_success() {
        let mut test = TestData::default();
        test.sender_amount += 1;
        let pay_result = Ok(test.preimage.to_vec());

        // Note the fee scales with the sender's overpayment
        let max_fee_msat = test.sender_amount - test.invoice_amount.unwrap();
        let expected_req = PaymentRequest {
            bolt11: test.invoice_string(),
            payment_hash: test.payment_hash(),
            amount_msat: None,
            max_fee_msat,
            max_cltv_delta: test.policy.cltv_expiry_delta - test.cltv_delta,
        };
        test.payment_provider
            .expect_pay()
            .with(eq(expected_req))
            .return_once(|_| pay_result)
            .once();
        let preimage = test.preimage.to_vec();
        let request = test.request();
        let manager = test.htlc_manager();

        let result = manager.handle_htlc(&request).await;

        assert!(
            matches!(result, HtlcAcceptedResponse::Resolve { payment_key } if payment_key.eq(&preimage))
        );
        let payments = manager.payments.lock().await;
        assert_eq!(0, payments.len());
    }

    #[tokio::test]
    #[traced_test]
    async fn test_single_htlc_payment_failure() {
        let mut test = TestData::default();
        let pay_result = Err(anyhow!("payment failed"));
        test.payment_provider
            .expect_pay()
            .return_once(|_| pay_result)
            .once();
        test.store
            .expect_mark_failed()
            .return_once(|_, _| Ok(()))
            .once();
        test.notification_service
            .expect_notify_payment_failed()
            .return_once(|_| ())
            .once();
        let request = test.request();
        let failure = test.temporary_trampoline_failure();
        let manager = test.htlc_manager();

        let result = manager.handle_htlc(&request).await;

        assert!(
            matches!(result, HtlcAcceptedResponse::Fail { failure_message } if failure_message.eq(&failure))
        );
        let payments = manager.payments.lock().await;
        assert_eq!(0, payments.len());
    }

    #[tokio::test]
    #[traced_test]
    async fn test_single_htlc_too_low_times_out() {
        let mut test = TestData::default();
        test.sender_amount -= 1;
        let request = test.request();
        let mpp_timeout = test.mpp_timeout;
        let failure = test.temporary_trampoline_failure();
        test.payment_provider.expect_pay().never();
        let manager = test.htlc_manager();

        let start = tokio::time::Instant::now();
        let result = manager.handle_htlc(&request).await;
        let end = tokio::time::Instant::now();

        let elapsed = end - start;
        assert!(elapsed.ge(&mpp_timeout));
        assert!(
            matches!(result, HtlcAcceptedResponse::Fail { failure_message } if failure_message.eq(&failure))
        );
        let payments = manager.payments.lock().await;
        assert_eq!(0, payments.len());
    }

    #[tokio::test]
    #[traced_test]
    async fn test_two_htlc_success() {
        let mut test1 = TestData::default();
        test1.sender_amount = 1;
        let request1 = test1.request();
        let mut test2 = TestData::default();
        test2.sender_amount -= 1;
        let request2 = test2.request();
        let preimage = test1.preimage.to_vec();
        let pay_result = Ok(test1.preimage.to_vec());
        test1
            .payment_provider
            .expect_pay()
            .return_once(|_| pay_result)
            .once();
        let manager = test1.htlc_manager();

        let (result1, result2) = join!(
            manager.handle_htlc(&request1),
            manager.handle_htlc(&request2)
        );

        assert!(
            matches!(result1, HtlcAcceptedResponse::Resolve { payment_key } if payment_key.eq(&preimage))
        );
        assert!(
            matches!(result2, HtlcAcceptedResponse::Resolve { payment_key } if payment_key.eq(&preimage))
        );
        let payments = manager.payments.lock().await;
        assert_eq!(0, payments.len());
    }

    #[tokio::test]
    #[traced_test]
    async fn test_two_htlc_failure() {
        let mut test1 = TestData::default();
        test1.sender_amount = 1;
        let request1 = test1.request();
        let mut test2 = TestData::default();
        test2.sender_amount -= 1;
        let request2 = test2.request();
        let failure = test1.temporary_trampoline_failure();
        let pay_result = Err(anyhow!("Payment failed"));
        test1
            .payment_provider
            .expect_pay()
            .return_once(|_| pay_result)
            .once();
        let manager = test1.htlc_manager();

        let (result1, result2) = join!(
            manager.handle_htlc(&request1),
            manager.handle_htlc(&request2)
        );

        assert!(
            matches!(result1, HtlcAcceptedResponse::Fail { failure_message } if failure_message.eq(&failure))
        );
        assert!(
            matches!(result2, HtlcAcceptedResponse::Fail { failure_message } if failure_message.eq(&failure))
        );
        let payments = manager.payments.lock().await;
        assert_eq!(0, payments.len());
    }

    #[tokio::test]
    #[traced_test]
    async fn test_two_htlcs_different_preimage_times_out() {
        let mut test1 = TestData::default();
        test1.sender_amount -= 1;
        let request1 = test1.request();
        let mut test2 = TestData::default();
        test2.preimage = preimage2();
        test2.sender_amount -= 1;
        let request2 = test2.request();
        let mpp_timeout = test1.mpp_timeout;
        let failure = test1.temporary_trampoline_failure();
        test1.payment_provider.expect_pay().never();
        let manager = test1.htlc_manager();

        let start = tokio::time::Instant::now();
        let (result1, result2) = join!(
            manager.handle_htlc(&request1),
            manager.handle_htlc(&request2)
        );
        let end = tokio::time::Instant::now();

        let elapsed = end - start;
        assert!(elapsed.ge(&mpp_timeout));
        assert!(
            matches!(result1, HtlcAcceptedResponse::Fail { failure_message } if failure_message.eq(&failure))
        );
        assert!(
            matches!(result2, HtlcAcceptedResponse::Fail { failure_message } if failure_message.eq(&failure))
        );
        let payments = manager.payments.lock().await;
        assert_eq!(0, payments.len());
    }

    #[tokio::test]
    #[traced_test]
    async fn test_replay_payment_in_flight_success() {
        let mut test = TestData::default();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 1;
        let preimage = test.preimage.to_vec();
        test.store = MockDatastore::new();
        test.store
            .expect_fetch_payment_info()
            .return_once(move |_| {
                Ok(store::PaymentState::Pending {
                    attempt_id: AttemptId::default(),
                    attempt_time_seconds: now,
                })
            })
            .once();
        test.store.expect_mark_succeeded().once();
        test.payment_provider
            .expect_wait_payment()
            .return_once(|_| Ok(Some(preimage)))
            .once();
        test.payment_provider.expect_pay().never();
        let preimage = test.preimage.to_vec();
        let request = test.request();
        let manager = test.htlc_manager();

        let result = manager.handle_htlc(&request).await;

        assert!(
            matches!(result, HtlcAcceptedResponse::Resolve { payment_key } if payment_key.eq(&preimage))
        );
        let payments = manager.payments.lock().await;
        assert_eq!(0, payments.len());
    }

    #[tokio::test]
    #[traced_test]
    async fn test_replay_payment_in_flight_failure() {
        let mut test = TestData::default();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            - test.mpp_timeout.as_secs()
            - 1;
        test.store = MockDatastore::new();
        test.store
            .expect_fetch_payment_info()
            .return_once(move |_| {
                Ok(store::PaymentState::Pending {
                    attempt_id: AttemptId::default(),
                    attempt_time_seconds: now,
                })
            })
            .once();
        test.store
            .expect_mark_failed()
            .return_once(|_, _| Ok(()))
            .once();
        test.payment_provider
            .expect_wait_payment()
            .return_once(|_| Ok(None))
            .once();
        test.payment_provider.expect_pay().never();
        let request = test.request();
        let failure = test.temporary_trampoline_failure();
        let manager = test.htlc_manager();

        let result = manager.handle_htlc(&request).await;

        assert!(
            matches!(result, HtlcAcceptedResponse::Fail { failure_message } if failure_message.eq(&failure))
        );
        let payments = manager.payments.lock().await;
        assert_eq!(0, payments.len());
    }

    #[tokio::test]
    #[traced_test]
    async fn test_replay_payment_in_flight_failure_then_success() {
        let mut test = TestData::default();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 1;
        let preimage = test.preimage.to_vec();
        test.store = MockDatastore::new();
        test.store
            .expect_fetch_payment_info()
            .return_once(move |_| {
                Ok(store::PaymentState::Pending {
                    attempt_id: AttemptId::default(),
                    attempt_time_seconds: now,
                })
            })
            .once();
        test.store
            .expect_mark_failed()
            .return_once(|_, _| Ok(()))
            .once();
        test.store
            .expect_add_payment_attempt()
            .return_once(|_| Ok(AttemptId::default()))
            .once();
        test.store
            .expect_mark_succeeded()
            .return_once(|_, _, _| Ok(()))
            .once();
        test.payment_provider
            .expect_wait_payment()
            .return_once(|_| Ok(None))
            .once();
        test.payment_provider
            .expect_pay()
            .return_once(|_| Ok(preimage))
            .once();
        let request = test.request();
        let preimage = test.preimage.to_vec();
        let manager = test.htlc_manager();

        let result = manager.handle_htlc(&request).await;

        assert!(
            matches!(result, HtlcAcceptedResponse::Resolve { payment_key } if payment_key.eq(&preimage))
        );
        let payments = manager.payments.lock().await;
        assert_eq!(0, payments.len());
    }

    #[tokio::test]
    #[traced_test]
    async fn test_self_route_hint_allowed() {
        let mut test = TestData::default();
        test.allow_self_route_hints = true;
        test.set_self_route_hint();
        let request = test.request();
        let preimage = test.preimage.to_vec();
        test.payment_provider
            .expect_pay()
            .return_once(|_| Ok(preimage))
            .once();
        let preimage = test.preimage.to_vec();
        let manager = test.htlc_manager();

        let result = manager.handle_htlc(&request).await;

        assert!(
            matches!(result, HtlcAcceptedResponse::Resolve { payment_key } if payment_key.eq(&preimage))
        );
        let payments = manager.payments.lock().await;
        assert_eq!(0, payments.len());
    }

    #[tokio::test]
    #[traced_test]
    async fn test_self_route_hint_disallowed() {
        let mut test = TestData::default();
        test.allow_self_route_hints = false;
        test.set_self_route_hint();
        let request = test.request();
        let failure = HtlcFailReason::TemporaryNodeFailure.encode();
        let manager = test.htlc_manager();

        let result = manager.handle_htlc(&request).await;

        assert!(
            matches!(result, HtlcAcceptedResponse::Fail { failure_message } if failure_message.eq(&failure))
        );
        let payments = manager.payments.lock().await;
        assert_eq!(0, payments.len());
    }

    #[tokio::test]
    #[traced_test]
    async fn test_invalid_invoice() {
        let test = TestData::default();
        let mut request = test.request();
        request.onion.payload = vec![TlvEntry {
            typ: 33001,
            value: "lnbc1".as_bytes().to_vec(),
        }]
        .into();
        let manager = test.htlc_manager();

        let result = manager.handle_htlc(&request).await;

        assert!(matches!(result, HtlcAcceptedResponse::Continue { payload } if payload.eq(&None)));
        let payments = manager.payments.lock().await;
        assert_eq!(0, payments.len());
    }

    #[tokio::test]
    #[traced_test]
    async fn test_invalid_invoice_invalid_utf8() {
        let test = TestData::default();
        let mut request = test.request();
        request.onion.payload = vec![TlvEntry {
            typ: 33001,
            value: vec![0xc3, 0x28],
        }]
        .into();
        let manager = test.htlc_manager();

        let result = manager.handle_htlc(&request).await;

        assert!(matches!(result, HtlcAcceptedResponse::Continue { payload } if payload.eq(&None)));
        let payments = manager.payments.lock().await;
        assert_eq!(0, payments.len());
    }

    #[tokio::test]
    #[traced_test]
    async fn test_zero_amount_invoice_with_amount() {
        let mut test = TestData::default();
        test.invoice_amount = None;
        let mut request = test.request();
        request.onion.payload = construct_payload(vec![
            TlvEntry {
                typ: 33001,
                value: test.invoice_bytes(),
            },
            TlvEntry {
                typ: 33003,
                value: 1_000_000u64.to_be_bytes().to_vec(),
            },
        ]);
        let preimage = test.preimage.to_vec();
        let pay_req = PaymentRequest {
            amount_msat: Some(1_000_000),
            bolt11: test.invoice_string(),
            max_cltv_delta: test.policy.cltv_expiry_delta - test.cltv_delta,
            max_fee_msat: test.sender_amount - 1_000_000,
            payment_hash: test.payment_hash(),
        };
        test.payment_provider
            .expect_pay()
            .with(eq(pay_req))
            .return_once(|_| Ok(preimage))
            .once();
        let preimage = test.preimage.to_vec();
        let manager = test.htlc_manager();

        let result = manager.handle_htlc(&request).await;

        assert!(
            matches!(result, HtlcAcceptedResponse::Resolve { payment_key } if payment_key.eq(&preimage))
        );
        let payments = manager.payments.lock().await;
        assert_eq!(0, payments.len());
    }

    #[tokio::test]
    #[traced_test]
    async fn test_zero_amount_invoice_without_amount() {
        let mut test = TestData::default();
        test.invoice_amount = None;
        let request = test.request();
        test.payment_provider.expect_pay().never();
        let manager = test.htlc_manager();

        let result = manager.handle_htlc(&request).await;

        assert!(matches!(result, HtlcAcceptedResponse::Continue { payload } if payload.eq(&None)));
        let payments = manager.payments.lock().await;
        assert_eq!(0, payments.len());
    }

    #[tokio::test]
    #[traced_test]
    async fn test_fee_insufficient() {
        let mut test = TestData::default();
        test.total_amount -= 1;
        let request = test.request();
        test.payment_provider.expect_pay().never();
        let failure = test.trampoline_fee_or_expiry_insufficient();
        let manager = test.htlc_manager();

        let result = manager.handle_htlc(&request).await;

        assert!(
            matches!(result, HtlcAcceptedResponse::Fail { failure_message } if failure_message.eq(&failure))
        );
        let payments = manager.payments.lock().await;
        assert_eq!(0, payments.len());
    }

    #[tokio::test]
    #[traced_test]
    async fn test_expiry_insufficient() {
        let mut test = TestData::default();
        let mut request = test.request();
        request.htlc.cltv_expiry_relative -= 1;
        test.payment_provider.expect_pay().never();
        let failure = test.trampoline_fee_or_expiry_insufficient();
        let manager = test.htlc_manager();

        let result = manager.handle_htlc(&request).await;

        assert!(
            matches!(result, HtlcAcceptedResponse::Fail { failure_message } if failure_message.eq(&failure))
        );
        let payments = manager.payments.lock().await;
        assert_eq!(0, payments.len());
    }
}
