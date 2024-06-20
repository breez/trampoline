use std::{sync::Arc, time::Duration};

use anyhow::Error;
use cln_plugin::options::ConfigOption;
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

const OPTION_CLTV_EXPIRY_DELTA: &str = "trampoline-cltv-expiry-delta";
const OPTION_FEE_BASE_MSAT: &str = "trampoline-fee-base-msat";
const OPTION_FEE_PPM: &str = "trampoline-fee-ppm";
const OPTION_MPP_TIMEOUT: &str = "trampoline-mpp-timeout";
const OPTION_NO_SELF_ROUTE_HINTS: &str = "trampoline-no-self-route-hints";
#[tokio::main]
async fn main() -> Result<(), Error> {
    // TODO: Find a sane default for the cltv expiry delta.
    let option_cltv_expiry_delta = ConfigOption::new_i64_with_default(
        OPTION_CLTV_EXPIRY_DELTA,
        576,
        "Cltv expiry delta for the trampoline routing policy. Any routes
        where the total cltv delta is lower than this number will not be
        tried.",
    );
    // TODO: A zero base fee default may exclude many routes for small payments.
    let option_fee_base_msat = ConfigOption::new_i64_with_default(
        OPTION_FEE_BASE_MSAT,
        0,
        "Base fee in millisatoshi charged for trampoline payments.",
    );
    let option_fee_ppm = ConfigOption::new_i64_with_default(
        OPTION_FEE_PPM,
        5000,
        "Fee rate in parts per million charges for trampoline payments.",
    );
    let option_mpp_timeout = ConfigOption::new_i64_with_default(
        OPTION_MPP_TIMEOUT,
        60,
        "Timeout in seconds before multipart htlcs that don't add up to the
        payment amount are failed back to the sender.",
    );
    let option_no_self_route_hints = ConfigOption::new_flag(
        OPTION_NO_SELF_ROUTE_HINTS,
        "If this flag is set, invoices where the current node is in an 
        invoice route hint are not supported. This can be useful if there
        are other important plugins acting only on forwards. The trampoline
        plugin will 'receive' and 'pay', so has different dynamics.",
    );

    let builder = plugin::init::<PayPaymentProvider>()
        .option(option_cltv_expiry_delta.clone())
        .option(option_fee_base_msat.clone())
        .option(option_fee_ppm.clone())
        .option(option_mpp_timeout.clone())
        .option(option_no_self_route_hints.clone());

    let cp = match builder.configure().await? {
        Some(cp) => cp,
        None => return Ok(()),
    };

    let rpc_file = cp.configuration().rpc_file;
    let mut rpc = cln_rpc::ClnRpc::new(rpc_file.clone()).await?;
    let info: GetinfoResponse = rpc.call_typed(&GetinfoRequest {}).await?;

    let cltv_expiry_delta = cp.option(&option_cltv_expiry_delta)?.try_into()?;
    let fee_base_msat = cp.option(&option_fee_base_msat)?.try_into()?;
    let fee_proportional_millionths = cp.option(&option_fee_ppm)?.try_into()?;
    let mpp_timeout_secs = cp.option(&option_mpp_timeout)?.try_into()?;
    let allow_self_route_hints: bool = !cp.option(&option_no_self_route_hints)?.try_into()?;
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
