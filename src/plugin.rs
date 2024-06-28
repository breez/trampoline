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

async fn on_block_added<P>(plugin: Plugin<PluginState<P>>, v: Value) -> Result<(), anyhow::Error>
where
    P: PaymentProvider + Clone + Send + Sync + 'static,
{
    let block_added: BlockAddedNotification = serde_json::from_value(v)?;
    plugin
        .state()
        .block_watcher
        .new_block(&block_added.block_added)
        .await;
    Ok(())
}
