use std::sync::Arc;

use crate::block_watcher::BlockWatcher;
use crate::cln_plugin::{Builder, Plugin};
use crate::messages::BlockAddedNotification;
use crate::store::ClnDatastore;
use crate::{
    htlc_manager::HtlcManager, messages::HtlcAcceptedRequest, payment_provider::PaymentProvider,
};
use serde_json::Value;
use tokio::io::{Stdin, Stdout};
use tracing::error;

const TRAMPOLINE_FEATURE_BIT: &str = "080000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000";

#[derive(Clone)]
pub struct PluginState<P>
where
    P: PaymentProvider,
{
    block_watcher: Arc<BlockWatcher>,
    htlc_manager: Arc<HtlcManager<BlockWatcher, P, ClnDatastore>>,
}

impl<P> PluginState<P>
where
    P: PaymentProvider,
{
    pub fn new(
        block_watcher: Arc<BlockWatcher>,
        htlc_manager: Arc<HtlcManager<BlockWatcher, P, ClnDatastore>>,
    ) -> Self {
        Self {
            block_watcher,
            htlc_manager,
        }
    }
}

/// Initializes the plugin builder with the appropriate hooks.
pub fn init<P>() -> Builder<PluginState<P>, Stdin, Stdout>
where
    P: PaymentProvider + Clone + Send + Sync + 'static,
{
    Builder::new(tokio::io::stdin(), tokio::io::stdout())
        .hook("htlc_accepted", on_htlc_accepted)
        .subscribe("block_added", on_block_added)
        .featurebits(
            crate::cln_plugin::FeatureBitsKind::Init,
            String::from(TRAMPOLINE_FEATURE_BIT),
        )
}

/// `htlc_accepted` hook invoked by core lightning.
async fn on_htlc_accepted<P>(
    plugin: Plugin<PluginState<P>>,
    v: Value,
) -> Result<Value, anyhow::Error>
where
    P: PaymentProvider + Clone + Send + Sync + 'static,
{
    let req: HtlcAcceptedRequest = match serde_json::from_value(v) {
        Ok(req) => req,
        Err(e) => {
            error!("failed to deserialize htlc accepted request: {:?}", e);
            return Err(e.into());
        }
    };
    let resp = plugin.state().htlc_manager.handle_htlc(&req).await;
    Ok(serde_json::to_value(resp)?)
}

/// `block_added` hook invoked by core lightning.
async fn on_block_added<P>(plugin: Plugin<PluginState<P>>, v: Value) -> Result<(), anyhow::Error>
where
    P: PaymentProvider + Clone + Send + Sync + 'static,
{
    let block_added: BlockAddedNotification = match serde_json::from_value(v) {
        Ok(req) => req,
        Err(e) => {
            error!("failed to deserialize block added request: {:?}", e);
            return Err(e.into());
        }
    };
    plugin
        .state()
        .block_watcher
        .new_block(&block_added.block_added)
        .await;
    Ok(())
}
