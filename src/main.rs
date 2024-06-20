use std::{sync::Arc, time::Duration};

use anyhow::Error;
use cln_plugin::options::{ConfigOption, DefaultIntegerConfigOption, FlagConfigOption};
use cln_rpc::model::{requests::GetinfoRequest, responses::GetinfoResponse};
use htlc_manager::HtlcManager;
use messages::TrampolineRoutingPolicy;
use payment_provider::PayPaymentProvider;
use plugin::PluginState;
use tracing::info;

mod cln_plugin;
mod htlc_manager;
mod messages;
mod payment_provider;
mod plugin;
mod tlv;

// TODO: Find a sane default for the cltv expiry delta.
const OPTION_CLTV_EXPIRY_DELTA: DefaultIntegerConfigOption = ConfigOption::new_i64_with_default(
    "trampoline-cltv-expiry-delta",
    576,
    "Cltv expiry delta for the trampoline routing policy. Any routes
    where the total cltv delta is lower than this number will not be
    tried.",
);
// TODO: A zero base fee default may exclude many routes for small payments.
const OPTION_FEE_BASE_MSAT: DefaultIntegerConfigOption = ConfigOption::new_i64_with_default(
    "trampoline-fee-base-msat",
    0,
    "Base fee in millisatoshi charged for trampoline payments.",
);
const OPTION_FEE_PPM: DefaultIntegerConfigOption = ConfigOption::new_i64_with_default(
    "trampoline-fee-ppm",
    5000,
    "Fee rate in parts per million charges for trampoline payments.",
);
const OPTION_MPP_TIMEOUT: DefaultIntegerConfigOption = ConfigOption::new_i64_with_default(
    "trampoline-mpp-timeout",
    60,
    "Timeout in seconds before multipart htlcs that don't add up to the
    payment amount are failed back to the sender.",
);
const OPTION_NO_SELF_ROUTE_HINTS: FlagConfigOption = ConfigOption::new_flag(
    "trampoline-no-self-route-hints",
    "If this flag is set, invoices where the current node is in an 
    invoice route hint are not supported. This can be useful if there
    are other important plugins acting only on forwards. The trampoline
    plugin will 'receive' and 'pay', so has different dynamics.",
);

#[tokio::main]
async fn main() -> Result<(), Error> {
    let builder = plugin::init::<PayPaymentProvider>()
        .option(OPTION_CLTV_EXPIRY_DELTA.clone())
        .option(OPTION_FEE_BASE_MSAT.clone())
        .option(OPTION_FEE_PPM.clone())
        .option(OPTION_MPP_TIMEOUT.clone())
        .option(OPTION_NO_SELF_ROUTE_HINTS.clone());

    let cp = match builder.configure().await? {
        Some(cp) => cp,
        None => return Ok(()),
    };

    let rpc_file = cp.configuration().rpc_file;
    let mut rpc = cln_rpc::ClnRpc::new(rpc_file.clone()).await?;
    let info: GetinfoResponse = rpc.call_typed(&GetinfoRequest {}).await?;

    let cltv_expiry_delta = cp.option(&OPTION_CLTV_EXPIRY_DELTA)?.try_into()?;
    let fee_base_msat = cp.option(&OPTION_FEE_BASE_MSAT)?.try_into()?;
    let fee_proportional_millionths = cp.option(&OPTION_FEE_PPM)?.try_into()?;
    let mpp_timeout_secs = cp.option(&OPTION_MPP_TIMEOUT)?.try_into()?;
    let allow_self_route_hints: bool = !cp.option(&OPTION_NO_SELF_ROUTE_HINTS)?;
    let policy = TrampolineRoutingPolicy {
        cltv_expiry_delta,
        fee_base_msat,
        fee_proportional_millionths,
    };

    let mpp_timeout = Duration::from_secs(mpp_timeout_secs);
    let payment_provider = Arc::new(PayPaymentProvider::new(rpc_file));
    let htlc_manager = HtlcManager::new(
        info.id,
        policy,
        mpp_timeout,
        payment_provider,
        allow_self_route_hints,
    );
    let state = PluginState::new(htlc_manager);
    let plugin = cp.start(state.clone()).await?;

    info!("Trampoline plugin started");

    plugin.join().await?;
    Ok(())
}
