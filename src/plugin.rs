use std::sync::Arc;

use crate::{
    htlc_manager::HtlcManager, messages::HtlcAcceptedRequest, payment_provider::PaymentProvider,
};
use crate::cln_plugin::{Builder, Plugin};
use serde_json::Value;
use tokio::io::{Stdin, Stdout};

#[derive(Clone)]
pub struct PluginState<P>
where
    P: PaymentProvider,
{
    htlc_manager: Arc<HtlcManager<P>>,
}

impl<P> PluginState<P>
where
    P: PaymentProvider,
{
    // TODO: initialize the state.
    pub fn new(htlc_manager: HtlcManager<P>) -> Self {
        Self {
            htlc_manager: Arc::new(htlc_manager),
        }
    }
}

pub fn init<P>() -> Builder<PluginState<P>, Stdin, Stdout>
where
    P: PaymentProvider + Clone + Send + Sync + 'static,
{
    Builder::new(tokio::io::stdin(), tokio::io::stdout()).hook("htlc_accepted", on_htlc_accepted)
}

async fn on_htlc_accepted<P>(
    plugin: Plugin<PluginState<P>>,
    v: Value,
) -> Result<Value, anyhow::Error>
where
    P: PaymentProvider + Clone + Send + Sync + 'static,
{
    let req: HtlcAcceptedRequest = serde_json::from_value(v)?;
    let resp = plugin.state().htlc_manager.handle_htlc(&req).await;
    Ok(serde_json::to_value(resp)?)
}
