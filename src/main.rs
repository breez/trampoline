use std::{sync::Arc, time::Duration};

use anyhow::Error;
use cln_rpc::model::{requests::GetinfoRequest, responses::GetinfoResponse};
use htlc_manager::HtlcManager;
use messages::TrampolineRoutingPolicy;
use payment_provider::PayPaymentProvider;
use plugin::PluginState;
use tracing::info;

mod htlc_manager;
mod messages;
mod payment_provider;
mod plugin;
mod tlv;

#[tokio::main]
async fn main() -> Result<(), Error> {
    let builder = plugin::init::<PayPaymentProvider>();
    let cp = match builder.configure().await? {
        Some(cp) => cp,
        None => return Ok(()),
    };

    let rpc_file = cp.configuration().rpc_file;
    let mut rpc = cln_rpc::ClnRpc::new(rpc_file.clone()).await?;
    let info: GetinfoResponse = rpc.call_typed(&GetinfoRequest {}).await?;

    // TODO: Make the policy configurable
    let policy = TrampolineRoutingPolicy {
        // TODO: What is a good cltv expiry delta? Our cltv delta basically
        // constraints the routes we use to pay. Because the entire route should
        // be encompassed in our delta.
        cltv_expiry_delta: 576,
        fee_base_msat: 0,
        fee_proportional_millionths: 5000,
    };

    // TODO: Make mpp timeout configurable
    let mpp_timeout = Duration::from_secs(60);
    let payment_provider = Arc::new(PayPaymentProvider::new(rpc_file));
    let htlc_manager = HtlcManager::new(info.id, policy, mpp_timeout, payment_provider);
    let state = PluginState::new(htlc_manager);
    let plugin = cp.start(state.clone()).await?;

    info!("Trampoline plugin started");

    plugin.join().await?;
    Ok(())
}
