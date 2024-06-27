use std::{sync::Arc, time::Duration};

use anyhow::{anyhow, Error};
use block_watcher::BlockWatcher;
use cln_plugin::options::{ConfigOption, DefaultIntegerConfigOption, FlagConfigOption};
use htlc_manager::{HtlcManager, HtlcManagerParams};
use messages::TrampolineRoutingPolicy;
use payment_provider::PayPaymentProvider;
use plugin::PluginState;
use rpc::{ClnRpc, Rpc};
use store::ClnDatastore;
use tokio::sync::mpsc;
use tracing::info;

mod block_watcher;
mod cln_plugin;
mod htlc_manager;
mod messages;
mod payment_provider;
mod plugin;
mod rpc;
mod store;
mod tlv;

const NAME_CLTV_DELTA: &str = "trampoline-cltv-delta";
const OPTION_CLTV_DELTA: DefaultIntegerConfigOption = ConfigOption::new_i64_with_default(
    NAME_CLTV_DELTA,
    34,
    "The number of blocks between incoming payments and outgoing payments: \
    this needs to be enough to make sure that if we have to, we can close the \
    outgoing payment before the incoming, or redeem the incoming once the \
    outgoing is redeemed.",
);
// TODO: Find a sane default for the cltv expiry delta.
const NAME_POLICY_CLTV_DELTA: &str = "trampoline-policy-cltv-delta";
const OPTION_POLICY_CLTV_DELTA: DefaultIntegerConfigOption = ConfigOption::new_i64_with_default(
    NAME_POLICY_CLTV_DELTA,
    576,
    "Cltv expiry delta for the trampoline routing policy. Any routes where the \
    total cltv delta is lower than this number will not be tried.",
);
// TODO: A zero base fee default may exclude many routes for small payments.
const OPTION_POLICY_FEE_BASE: DefaultIntegerConfigOption = ConfigOption::new_i64_with_default(
    "trampoline-policy-fee-base",
    0,
    "The base fee to charge for every trampoline payment which passes through.",
);
const OPTION_POLICY_FEE_PER_SATOSHI: DefaultIntegerConfigOption =
    ConfigOption::new_i64_with_default(
        "trampoline-policy-fee-per-satoshi",
        5000,
        "This is the proportional fee to charge for every trampoline payment which \
    passes through. As percentages are too coarse, it's in millionths, so \
    10000 is 1%, 1000 is 0.1%.",
    );
const OPTION_MPP_TIMEOUT: DefaultIntegerConfigOption = ConfigOption::new_i64_with_default(
    "trampoline-mpp-timeout",
    60,
    "Timeout in seconds before multipart htlcs that don't add up to the \
    payment amount are failed back to the sender.",
);
const OPTION_NO_SELF_ROUTE_HINTS: FlagConfigOption = ConfigOption::new_flag(
    "trampoline-no-self-route-hints",
    "If this flag is set, invoices where the current node is in an invoice \
    route hint are not supported. This can be useful if there are other \
    important plugins acting only on forwards. The trampoline plugin will \
    'receive' and 'pay', so has different dynamics.",
);

#[tokio::main]
async fn main() -> Result<(), Error> {
    let builder = plugin::init::<PayPaymentProvider>()
        .option(OPTION_CLTV_DELTA)
        .option(OPTION_POLICY_CLTV_DELTA)
        .option(OPTION_POLICY_FEE_BASE)
        .option(OPTION_POLICY_FEE_PER_SATOSHI)
        .option(OPTION_MPP_TIMEOUT)
        .option(OPTION_NO_SELF_ROUTE_HINTS);

    let cp = match builder.configure().await? {
        Some(cp) => cp,
        None => return Ok(()),
    };

    let rpc_file = cp.configuration().rpc_file;
    let rpc = Arc::new(Rpc::new(rpc_file.clone()));
    let info = rpc.get_info().await?;

    let cltv_delta = cp.option(&OPTION_CLTV_DELTA)?.try_into()?;
    let cltv_expiry_delta = cp.option(&OPTION_POLICY_CLTV_DELTA)?.try_into()?;
    if cltv_expiry_delta <= cltv_delta {
        return Err(anyhow!(
            "{} ({}) must be greater than {} ({})",
            NAME_POLICY_CLTV_DELTA,
            cltv_expiry_delta,
            NAME_CLTV_DELTA,
            cltv_delta,
        ));
    }
    let fee_base_msat = cp.option(&OPTION_POLICY_FEE_BASE)?.try_into()?;
    let fee_proportional_millionths = cp.option(&OPTION_POLICY_FEE_PER_SATOSHI)?.try_into()?;
    let mpp_timeout_secs = cp.option(&OPTION_MPP_TIMEOUT)?.try_into()?;
    let allow_self_route_hints: bool = !cp.option(&OPTION_NO_SELF_ROUTE_HINTS)?;
    let routing_policy = TrampolineRoutingPolicy {
        cltv_expiry_delta,
        fee_base_msat,
        fee_proportional_millionths,
    };

    let mpp_timeout = Duration::from_secs(mpp_timeout_secs);
    let payment_provider = Arc::new(PayPaymentProvider::new(Arc::clone(&rpc)));
    let mut block_watcher = BlockWatcher::new(Arc::clone(&rpc));
    let (sender, receiver) = mpsc::channel(1);
    let block_join = block_watcher.start(receiver).await?;
    let block_watcher = Arc::new(block_watcher);
    let store = Arc::new(ClnDatastore::new(Arc::clone(&rpc)));
    let htlc_manager = Arc::new(HtlcManager::new(HtlcManagerParams {
        allow_self_route_hints,
        block_provider: Arc::clone(&block_watcher),
        cltv_delta,
        local_pubkey: info.id,
        mpp_timeout,
        payment_provider,
        routing_policy,
        store: Arc::clone(&store),
    }));
    let state = PluginState::new(Arc::clone(&block_watcher), Arc::clone(&htlc_manager));
    let plugin = cp.start(state.clone()).await?;

    info!("Trampoline plugin started");

    plugin.join().await?;
    sender.send(()).await?;
    block_join.await?;
    Ok(())
}
